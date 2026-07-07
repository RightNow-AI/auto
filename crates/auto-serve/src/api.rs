//! The pure request core of the tier-1 server.
//!
//! [`handle`] is a total function from a parsed [`ApiRequest`] to an
//! [`ApiResponse`]; it never touches a socket. `server.rs` is the thin shell
//! that turns `tiny_http` requests into [`ApiRequest`]s and writes the
//! responses back. Keeping the routing and the guard-then-execute decision
//! here — over plain structs — is what lets every route be unit-tested
//! without binding a port (CLAUDE.md: verification is the product; these
//! tests never open a socket).
//!
//! Semantics mirror `auto run` (crates/auto-cli, `run_artifact`): a guarded
//! artifact evaluates its guard first; **proceed** runs tier-1, **trip**
//! ABSTAINS with a 409 — v0 has no in-server tier-0 (spec/runtime.md §8,
//! ADR-0011), exactly like `auto run` without `--tier0`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use auto_registry::{Registry, RegistryError};
use auto_runtime::executor::HostTools;
use auto_runtime::{Guard, GuardOutcome, WasmExecutor};
use auto_trace::model::canonical_json;
use serde_json::{Value, json};

use crate::ServeError;

/// The HTTP method the core routes on. The shell maps `tiny_http::Method`
/// into this and refuses (405) anything that is not one of these before it
/// ever reaches [`handle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// A parsed request: everything [`handle`] needs, nothing socket-shaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    pub method: Method,
    /// the requested resource; a `?query` suffix is tolerated and ignored by
    /// the router
    pub path: String,
    /// raw request body (the input JSON for `/run`)
    pub body: Vec<u8>,
}

/// A response: an HTTP status and a JSON body. The shell serializes `body`
/// and writes it as `application/json` under `status`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiResponse {
    pub status: u16,
    pub body: Value,
}

impl ApiResponse {
    fn json(status: u16, body: Value) -> Self {
        Self { status, body }
    }

    fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, json!({ "error": message.into() }))
    }
}

/// Parse the operator's `--tool name=command` flags into the server-wide Live
/// tool table, mirroring `auto run`'s grammar EXACTLY (crates/auto-cli
/// `parse_tool_table`): split once on `=`, the command split on whitespace,
/// name and command both non-empty. An empty flag list is no host at all
/// ([`None`]); a malformed flag is a startup error ([`ServeError::Config`]) so
/// the server fails loud before binding rather than serve half-configured.
pub(crate) fn parse_tool_table(flags: &[String]) -> Result<Option<HostTools>, ServeError> {
    if flags.is_empty() {
        return Ok(None);
    }
    let mut table = BTreeMap::new();
    for flag in flags {
        let Some((name, command)) = flag.split_once('=') else {
            return Err(ServeError::Config {
                detail: format!("--tool {flag:?} is not name=command"),
            });
        };
        let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
        if name.is_empty() || argv.is_empty() {
            return Err(ServeError::Config {
                detail: format!("--tool {flag:?} needs a name and a command"),
            });
        }
        table.insert(name.to_owned(), argv);
    }
    Ok(Some(HostTools::Live(table)))
}

/// Wrap a tool host's execution in a per-request budget + audit (ADR-0028).
///
/// Returns a [`HostTools::Callback`] over `names` whose closure, on each call:
/// increments the shared `counter`; refuses the call with an err string once
/// the running count exceeds `budget` (the artifact turns the err envelope into
/// an honest trap → the request's 500); otherwise logs one audit line
/// `tool audit: <tool> call #<k>` to stderr and delegates to `inner`. Only
/// executed calls are audited — a refused over-budget call never ran the tool,
/// so it is recorded by the err/500, not by an audit line implying a side
/// effect that did not happen.
///
/// The budget is per REQUEST, not per host: `counter` is reset to zero at the
/// top of every `/run` ([`ServerState::with_tool_policy`], reset in [`run`]).
/// This is correct only because the server is sequential (ADR-0011) — one
/// request touches the counter at a time. A concurrent server would share one
/// counter across in-flight requests and must move it into per-request state.
///
/// `inner` is the underlying execution the budget guards. In production it is a
/// Live subprocess ([`spawn_tool`]); tests pass a pure fake, so the budget and
/// audit logic is exercised without spawning a process.
pub(crate) fn budgeted(
    names: BTreeSet<String>,
    budget: u64,
    counter: Arc<AtomicU64>,
    mut inner: impl FnMut(&str, &Value) -> Result<Value, String> + Send + 'static,
) -> HostTools {
    HostTools::callback(names, move |name: &str, input: &Value| {
        let k = counter.fetch_add(1, Ordering::Relaxed) + 1;
        if k > budget {
            return Err(format!(
                "tool budget exceeded: {budget} per request (ADR-0028)"
            ));
        }
        eprintln!("tool audit: {name} call #{k}");
        inner(name, input)
    })
}

/// Execute one Live tool call: the tier-0 command contract (spec/runtime.md §3),
/// mirroring `auto-runtime`'s `HostTools::Live` — look the name up in the argv
/// table, append the canonical-JSON input as the final argument, require exit 0,
/// and parse stdout as the JSON output value.
///
/// auto-serve reimplements this one arm because the runtime does not expose Live
/// execution as a standalone public fn (only through `WasmExecutor::execute`),
/// and the budget wrapper ([`budgeted`]) must OWN the execution it counts.
/// Canonical JSON (via `auto-trace`) keeps the bytes a budgeted tool receives
/// identical to an unbudgeted Live tool — canonical text is the wire format
/// everywhere (ADR-0027). Kept byte-for-byte in step with the runtime arm; a
/// divergence is a bug.
fn spawn_tool(
    table: &BTreeMap<String, Vec<String>>,
    name: &str,
    input: &Value,
) -> Result<Value, String> {
    let argv = table
        .get(name)
        .ok_or_else(|| format!("tool {name:?} is not in the provided tool table"))?;
    let (command, args) = argv
        .split_first()
        .ok_or_else(|| format!("tool {name:?} has an empty command"))?;
    let output = std::process::Command::new(command)
        .args(args)
        .arg(canonical_json(input))
        .output()
        .map_err(|e| format!("tool {name:?} failed to spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tool {name:?} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(output.stdout.trim_ascii())
        .map_err(|e| format!("tool {name:?} stdout is not JSON: {e}"))
}

/// One loaded artifact's run-path essentials: the compiled module and the
/// parsed guard.
///
/// Nothing else from the artifact is read after load — the `/artifacts`
/// listing comes from the live registry, not this cache — so only these two
/// are retained (storing the whole `Artifact`/manifest would be dead weight).
struct Loaded {
    /// `None` when the artifact carries no `guard.json`: it runs tier-1
    /// unguarded, mirroring `auto run`.
    guard: Option<Guard>,
    executor: WasmExecutor,
}

/// Why an artifact could not be loaded for `/run`.
enum LoadError {
    /// no such id in the registry (absent, or not a 64-hex content id)
    NotFound,
    /// registry integrity/IO failure: tamper evidence (id mismatch, bad
    /// signature), a bad key, or a read error — a server-side problem, 500
    Registry(String),
    /// bytes parsed by the registry but not loadable as a runnable artifact
    /// (bad manifest, bad guard json, module rejected) — also 500
    Artifact(String),
}

/// Server state: the backing registry, the operator's server-wide tool table,
/// and a load-once cache of runnable artifacts keyed by content id.
///
/// The cache is sound because ids are content addresses: an id pins its
/// bytes, so a cached executor can never go stale (any change would land
/// under a different id). It is never evicted — a stated v0 limit (no hot
/// reload): an artifact deleted from the registry after first load keeps
/// answering from cache until restart (ADR-0011).
///
/// `tools` is the single Live table (ADR-0017): capability artifacts load
/// through it; pure artifacts load with NO host regardless (see [`Self::load_uncached`]).
/// With a per-request tool budget (ADR-0028) `tools` is instead the budgeted
/// Callback wrapper around that table.
pub struct ServerState {
    registry: Registry,
    /// the server-wide tool host (`Some` when the operator passed `--tool`
    /// flags), forwarded ONLY to capability artifacts; `None` = a pure server.
    /// When a per-request budget is set this is the budgeted Callback wrapper
    /// ([`budgeted`]) around the operator's Live table, not the bare table.
    tools: Option<HostTools>,
    /// per-request tool-call counter, shared with the budgeted host's closure
    /// and reset to zero at the top of every `/run` (ADR-0028). Sound because
    /// the server is sequential (ADR-0011): one request touches it at a time.
    /// Unused (stays zero) when no budget is configured.
    tool_calls: Arc<AtomicU64>,
    cache: BTreeMap<String, Loaded>,
}

impl ServerState {
    /// State for a **pure** server — no tool host, no budget. Capability
    /// artifacts refuse at load (surfaced as a 500 on `/run`); pure artifacts
    /// serve as before.
    pub fn new(registry: Registry) -> Self {
        Self::with_tool_policy(registry, None, None)
    }

    /// State with a server-wide tool host and no per-request budget — today's
    /// behavior (ADR-0017). Capability artifacts load through `tools`; pure
    /// artifacts still load with no host.
    pub fn with_tools(registry: Registry, tools: Option<HostTools>) -> Self {
        Self::with_tool_policy(registry, tools, None)
    }

    /// State with a server-wide tool host and an optional per-request tool-call
    /// budget (ADR-0028).
    ///
    /// `max_tool_calls_per_request = None` is exactly [`Self::with_tools`]:
    /// today's unlimited behavior, the operator's table forwarded as-is.
    /// `Some(n)` wraps a Live table in a counting [`HostTools::Callback`]
    /// ([`budgeted`]) that audits every executed tool call, refuses the
    /// `n+1`-th call in one request, and shares [`Self::tool_calls`] so [`run`]
    /// can reset the count per request. A budget with no `--tool` table has
    /// nothing to count; a non-Live host (never produced by the CLI) passes
    /// through unwrapped.
    pub fn with_tool_policy(
        registry: Registry,
        tools: Option<HostTools>,
        max_tool_calls_per_request: Option<u64>,
    ) -> Self {
        let tool_calls = Arc::new(AtomicU64::new(0));
        let tools = match (tools, max_tool_calls_per_request) {
            (Some(HostTools::Live(table)), Some(budget)) => {
                let names: BTreeSet<String> = table.keys().cloned().collect();
                let counter = Arc::clone(&tool_calls);
                Some(budgeted(names, budget, counter, move |name, input| {
                    spawn_tool(&table, name, input)
                }))
            }
            (other, _) => other,
        };
        Self {
            registry,
            tools,
            tool_calls,
            cache: BTreeMap::new(),
        }
    }

    /// Return the cached runnable artifact for `id`, loading it once on first
    /// use. Failures are not cached, so an artifact added after a miss loads
    /// on a later request.
    fn load(&mut self, id: &str) -> Result<&Loaded, LoadError> {
        if !self.cache.contains_key(id) {
            let loaded = self.load_uncached(id)?;
            self.cache.insert(id.to_owned(), loaded);
        }
        Ok(self
            .cache
            .get(id)
            .expect("just inserted or already present"))
    }

    /// Fetch verified bytes (via [`Registry::get`], which re-parses,
    /// recomputes the content id, and verifies any signature), then parse the
    /// artifact, its guard, and compile the module — the same sequence
    /// `auto run` performs before executing.
    fn load_uncached(&self, id: &str) -> Result<Loaded, LoadError> {
        let bytes = self.fetch_bytes(id)?;
        let artifact = auto_backend::Artifact::from_bytes(&bytes)
            .map_err(|e| LoadError::Artifact(e.to_string()))?;
        // parse + version-check the manifest so a broken artifact fails at
        // load, not mid-run (mirrors auto run); its capability list decides
        // whether the tool host applies.
        let manifest = artifact
            .manifest()
            .map_err(|e| LoadError::Artifact(e.to_string()))?;
        let guard = match artifact.entries.get(auto_backend::container::GUARD_ENTRY) {
            None => None,
            Some(raw) => {
                let text = std::str::from_utf8(raw)
                    .map_err(|_| LoadError::Artifact("guard is not utf-8".to_owned()))?;
                Some(Guard::from_json(text).map_err(|e| LoadError::Artifact(e.to_string()))?)
            }
        };
        // Pure artifacts (empty capabilities) load with NO host even when the
        // server carries a table — the loader refuses a host on a pure
        // artifact, so pure artifacts are COMPLETELY unaffected. Capability
        // artifacts load through the server table; the loader enforces
        // per-artifact coverage and, with no table, refuses naming the missing
        // tools — surfaced as a 500 on `/run` (ADR-0017).
        let tools = if manifest.capabilities.is_empty() {
            None
        } else {
            self.tools.clone()
        };
        let executor = WasmExecutor::from_artifact_with_tools(&artifact, tools)
            .map_err(|e| LoadError::Artifact(e.to_string()))?;
        Ok(Loaded { guard, executor })
    }

    /// [`Registry::get`] writes verified bytes to a path; receive them in a
    /// unique temp file, read them back, and remove it. (Mirrors the
    /// temp-file pattern in `auto-cli`; keeps `tempfile` a dev-only dependency
    /// and the runtime dependency set exactly as frozen.)
    fn fetch_bytes(&self, id: &str) -> Result<Vec<u8>, LoadError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("auto-serve-{}-{unique}.cbin", std::process::id()));
        let fetched = match self.registry.get(id, &tmp) {
            Ok(_verified) => std::fs::read(&tmp).map_err(|e| LoadError::Registry(e.to_string())),
            Err(RegistryError::NotFound { .. }) => Err(LoadError::NotFound),
            Err(other) => Err(LoadError::Registry(other.to_string())),
        };
        // get only writes on success; unconditional best-effort cleanup
        let _ = std::fs::remove_file(&tmp);
        fetched
    }

    fn artifact_count(&self) -> Result<usize, RegistryError> {
        Ok(self.registry.list()?.len())
    }
}

/// Route and answer one request. Total: every method/path lands on exactly
/// one response, and the run path mirrors `auto run`'s guard-then-execute
/// (proceed → tier-1; trip → abstain).
pub fn handle(state: &mut ServerState, request: &ApiRequest) -> ApiResponse {
    // a query string is not part of the route
    let path = request.path.split('?').next().unwrap_or("");

    if let Some(id) = path.strip_prefix("/run/") {
        return match request.method {
            Method::Post => run(state, id, &request.body),
            Method::Get => ApiResponse::error(405, "POST required for /run/<id>"),
        };
    }
    match path {
        "/health" => match request.method {
            Method::Get => health(state),
            Method::Post => ApiResponse::error(405, "GET required for /health"),
        },
        "/artifacts" => match request.method {
            Method::Get => artifacts(state),
            Method::Post => ApiResponse::error(405, "GET required for /artifacts"),
        },
        other => ApiResponse::error(404, format!("no route for {other}")),
    }
}

/// `GET /health` → `{"ok":true,"artifacts":<count>}`. The count is the live
/// registry listing; if the registry cannot be listed (e.g. a corrupt public
/// key) that is reported honestly as a 500 rather than a misleading `ok`.
fn health(state: &ServerState) -> ApiResponse {
    match state.artifact_count() {
        Ok(count) => ApiResponse::json(200, json!({ "ok": true, "artifacts": count })),
        Err(e) => ApiResponse::error(500, format!("registry list failed: {e}")),
    }
}

/// `GET /artifacts` → `{"artifacts":[{"id","task","scope"[,"problem"]}]}`,
/// straight from the live registry listing (not the run cache). A per-entry
/// `problem` (corrupt container, id mismatch, unreadable manifest) is
/// surfaced when present, never hidden behind a blank-but-valid-looking row.
fn artifacts(state: &ServerState) -> ApiResponse {
    match state.registry.list() {
        Ok(entries) => {
            let rows: Vec<Value> = entries
                .into_iter()
                .map(|entry| {
                    let mut row = serde_json::Map::new();
                    row.insert("id".to_owned(), json!(entry.id));
                    row.insert("task".to_owned(), json!(entry.task));
                    row.insert("scope".to_owned(), json!(entry.scope));
                    if let Some(problem) = entry.problem {
                        row.insert("problem".to_owned(), json!(problem));
                    }
                    Value::Object(row)
                })
                .collect();
            ApiResponse::json(200, json!({ "artifacts": rows }))
        }
        Err(e) => ApiResponse::error(500, format!("registry list failed: {e}")),
    }
}

/// `POST /run/<id>` — the guard-then-execute path, mirroring `auto run`.
///
/// Order mirrors `run_artifact`: load the artifact first (unknown id → 404,
/// registry/artifact fault → 500), then parse the body (bad JSON → 400), then
/// evaluate the guard (trip → 409 abstain), then execute tier-1 (trap/ABI
/// failure → 500). v0 does not conformance-check the input against the
/// manifest's declared type (auto run does): that check lives in `auto-contract`,
/// which auto-serve deliberately does not depend on — a guarded artifact trips
/// on a wrong-shaped input, and an unguarded one surfaces the module's own
/// result or failure. Recorded in ADR-0011.
fn run(state: &mut ServerState, id: &str, body: &[u8]) -> ApiResponse {
    // per-request tool budget resets HERE (ADR-0028): the shared counter the
    // budgeted host increments starts at zero for every request, so the budget
    // is per request, not per process. Sound because the server is sequential
    // (ADR-0011). A no-op when no budget/tool host is configured.
    state.tool_calls.store(0, Ordering::Relaxed);

    let loaded = match state.load(id) {
        Ok(loaded) => loaded,
        Err(LoadError::NotFound) => {
            return ApiResponse::error(404, format!("no artifact `{id}` in the registry"));
        }
        Err(LoadError::Registry(detail)) => {
            return ApiResponse::error(500, format!("registry: {detail}"));
        }
        Err(LoadError::Artifact(detail)) => {
            return ApiResponse::error(500, format!("artifact could not be loaded: {detail}"));
        }
    };

    let input: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(e) => return ApiResponse::error(400, format!("request body is not valid JSON: {e}")),
    };

    // guard first: a trip ABSTAINS (409). No in-server tier-0 in v0 — a
    // server-side deopt needs a per-request spend policy that does not exist
    // (ADR-0011).
    if let Some(guard) = &loaded.guard {
        match guard.evaluate(&input) {
            GuardOutcome::Proceed { .. } => {}
            GuardOutcome::Trip {
                reason,
                distance,
                threshold,
            } => {
                return ApiResponse::json(
                    409,
                    json!({
                        "abstained": true,
                        "reason": reason,
                        "distance": distance,
                        "threshold": threshold,
                    }),
                );
            }
        }
    }

    match loaded.executor.execute(&input) {
        Ok(output) => ApiResponse::json(200, json!({ "output": output })),
        Err(e) => ApiResponse::error(500, format!("tier-1 execution failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use auto_backend::container::GUARD_ENTRY;
    use auto_backend::{
        Artifact, MANIFEST_ENTRY, MANIFEST_VERSION, MODULE_ENTRY, Manifest, Measured, Provenance,
    };

    use super::*;

    /// A real, runnable module: bump-allocates from 4096 and echoes the input
    /// region back. wasmtime compiles this wat text directly, so `/run` tests
    /// exercise the true tier-1 path — not a fake (mirrors
    /// auto-runtime::executor's ECHO fixture).
    const ECHO: &str = r#"(module
        (memory (export "memory") 2)
        (global $next (mut i32) (i32.const 4096))
        (func (export "alloc") (param i32) (result i32)
            global.get $next
            global.get $next local.get 0 i32.add global.set $next)
        (func (export "run") (param i32 i32) (result i64)
            local.get 0 i64.extend_i32_u i64.const 32 i64.shl
            local.get 1 i64.extend_i32_u i64.or))"#;

    fn manifest() -> Manifest {
        manifest_caps(vec![])
    }

    fn manifest_caps(capabilities: Vec<String>) -> Manifest {
        Manifest {
            manifest_version: MANIFEST_VERSION,
            task: "toy-agent".into(),
            scope_kind: "model_call".into(),
            scope_name: "fake-frontier".into(),
            interface_input: "text".into(),
            interface_output: "text".into(),
            capabilities,
            contract_id: "c".repeat(8),
            eval_run_ids: vec!["run-1".into()],
            provenance: Provenance {
                trace_ids: vec!["0".repeat(32)],
                reference: "test reference".into(),
                observations: 1,
            },
            measured: Measured {
                compiled_latency_ms_p50: 1,
                compiled_latency_ms_p95: 2,
                compiled_latency_ms_max: 3,
                reference_recorded_latency_ms_p95: 40,
            },
            notes: String::new(),
        }
    }

    /// Container bytes for a runnable ECHO artifact, optionally guarded.
    fn artifact_bytes(guard: Option<&Guard>) -> Vec<u8> {
        let mut entries = BTreeMap::new();
        entries.insert(
            MANIFEST_ENTRY.to_owned(),
            manifest().canonical_json().into_bytes(),
        );
        entries.insert(MODULE_ENTRY.to_owned(), ECHO.as_bytes().to_vec());
        if let Some(guard) = guard {
            entries.insert(GUARD_ENTRY.to_owned(), guard.to_json().into_bytes());
        }
        Artifact::new(entries).to_bytes()
    }

    /// A registry in a fresh tempdir with one ECHO artifact added; returns the
    /// tempdir (kept alive), the server state, and the artifact's content id.
    fn setup(guard: Option<&Guard>) -> (tempfile::TempDir, ServerState, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::open(&dir.path().join("registry")).expect("open registry");
        let bytes = artifact_bytes(guard);
        let src = dir.path().join("artifact.cbin");
        std::fs::write(&src, &bytes).expect("write artifact file");
        let outcome = registry.add(&src, false).expect("add artifact");
        (dir, ServerState::new(registry), outcome.id)
    }

    /// A REAL capability artifact: the tool interpreter (auto.tool_call
    /// imported) carrying a pipeline-v1 payload of one `tool_call` stage,
    /// declaring capability `lookup`. Running it hands the whole input to the
    /// `lookup` tool and returns the tool's answer — the true tier-1 capability
    /// path (ADR-0017), not a stand-in.
    /// A capability artifact whose pipeline calls `lookup` exactly `calls`
    /// times (each tool stage feeds the next; `eval_pipeline` invokes the host
    /// once per stage). Declares capability `lookup`; running it invokes the
    /// tool `calls` times in one request — enough to cross a per-request budget.
    fn tool_artifact_calls(calls: usize) -> Vec<u8> {
        let stages = std::iter::repeat_with(|| auto_passes::auto_dsl::Stage::Tool {
            name: "lookup".into(),
        })
        .take(calls)
        .collect();
        let pipeline = auto_passes::auto_dsl::Pipeline::new(stages);
        let mut entries = BTreeMap::new();
        entries.insert(
            MANIFEST_ENTRY.to_owned(),
            manifest_caps(vec!["lookup".into()])
                .canonical_json()
                .into_bytes(),
        );
        entries.insert(
            MODULE_ENTRY.to_owned(),
            auto_passes::tool_interpreter_wasm().to_vec(),
        );
        entries.insert(
            auto_backend::container::PROGRAM_ENTRY.to_owned(),
            pipeline.to_json().into_bytes(),
        );
        Artifact::new(entries).to_bytes()
    }

    /// A registry in a fresh tempdir holding one capability artifact whose
    /// pipeline calls `lookup` `calls` times; returns the tempdir, the open
    /// registry, and the artifact's id.
    fn tool_registry(calls: usize) -> (tempfile::TempDir, Registry, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::open(&dir.path().join("registry")).expect("open registry");
        let src = dir.path().join("tool.cbin");
        std::fs::write(&src, tool_artifact_calls(calls)).expect("write artifact file");
        let id = registry.add(&src, false).expect("add artifact").id;
        (dir, registry, id)
    }

    /// A registry in a fresh tempdir holding one (single-call) capability
    /// artifact; returns the tempdir, server state built with `tools`, and id.
    fn setup_tool(tools: Option<HostTools>) -> (tempfile::TempDir, ServerState, String) {
        let (dir, registry, id) = tool_registry(1);
        (dir, ServerState::with_tools(registry, tools), id)
    }

    /// A replay host covering `lookup` for one witnessed `(input -> output)`
    /// pair, keyed exactly as the executor keys it (canonical JSON of the
    /// register the tool stage receives — here the whole pipeline input).
    fn lookup_replay(input: &Value, output: Value) -> HostTools {
        let mut recorded = BTreeMap::new();
        recorded.insert(
            (
                "lookup".to_owned(),
                auto_trace::model::canonical_json(input),
            ),
            output,
        );
        HostTools::Replay(recorded)
    }

    fn get(path: &str) -> ApiRequest {
        ApiRequest {
            method: Method::Get,
            path: path.to_owned(),
            body: Vec::new(),
        }
    }

    fn post(path: &str, body: &[u8]) -> ApiRequest {
        ApiRequest {
            method: Method::Post,
            path: path.to_owned(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn health_reports_the_artifact_count() {
        let (_dir, mut state, _id) = setup(None);
        let resp = handle(&mut state, &get("/health"));
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, json!({ "ok": true, "artifacts": 1 }));
    }

    #[test]
    fn health_on_empty_registry_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry")).unwrap();
        let mut state = ServerState::new(registry);
        let resp = handle(&mut state, &get("/health"));
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, json!({ "ok": true, "artifacts": 0 }));
    }

    #[test]
    fn health_tolerates_a_query_string() {
        let (_dir, mut state, _id) = setup(None);
        let resp = handle(&mut state, &get("/health?probe=1"));
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, json!({ "ok": true, "artifacts": 1 }));
    }

    #[test]
    fn artifacts_lists_id_task_scope() {
        let (_dir, mut state, id) = setup(None);
        let resp = handle(&mut state, &get("/artifacts"));
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.body,
            json!({
                "artifacts": [
                    { "id": id, "task": "toy-agent", "scope": "model_call(fake-frontier)" }
                ]
            })
        );
    }

    #[test]
    fn run_unguarded_echoes_the_input() {
        let (_dir, mut state, id) = setup(None);
        let resp = handle(&mut state, &post(&format!("/run/{id}"), br#"{"a":1}"#));
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, json!({ "output": { "a": 1 } }));
    }

    #[test]
    fn run_guarded_proceeds_on_a_witness() {
        let guard = Guard::build(&[json!("hello world")], None).unwrap();
        let (_dir, mut state, id) = setup(Some(&guard));
        let resp = handle(
            &mut state,
            &post(&format!("/run/{id}"), br#""hello world""#),
        );
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, json!({ "output": "hello world" }));
    }

    #[test]
    fn run_guarded_abstains_beyond_calibration() {
        let guard = Guard::build(&[json!("hello world")], None).unwrap();
        let (_dir, mut state, id) = setup(Some(&guard));
        let resp = handle(
            &mut state,
            &post(&format!("/run/{id}"), br#""nothing alike here""#),
        );
        assert_eq!(resp.status, 409);
        assert_eq!(resp.body["abstained"], json!(true));
        assert_eq!(resp.body["threshold"], json!(0.0));
        assert!(resp.body["distance"].as_f64().expect("distance number") > 0.0);
        assert!(resp.body["reason"].is_string());
    }

    #[test]
    fn run_guarded_wrong_shape_abstains_with_null_distance() {
        let guard = Guard::build(&[json!("hello world")], None).unwrap();
        let (_dir, mut state, id) = setup(Some(&guard));
        // an object where the guard requires a bare string: trips, no distance
        let resp = handle(
            &mut state,
            &post(&format!("/run/{id}"), br#"{"not":"text"}"#),
        );
        assert_eq!(resp.status, 409);
        assert_eq!(resp.body["abstained"], json!(true));
        assert_eq!(resp.body["distance"], Value::Null);
    }

    #[test]
    fn run_unknown_id_is_404() {
        let (_dir, mut state, _id) = setup(None);
        let resp = handle(
            &mut state,
            &post(&format!("/run/{}", "0".repeat(64)), b"null"),
        );
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn run_malformed_id_is_404() {
        let (_dir, mut state, _id) = setup(None);
        let resp = handle(&mut state, &post("/run/not-a-content-id", b"null"));
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn run_bad_json_body_is_400() {
        let (_dir, mut state, id) = setup(None);
        let resp = handle(&mut state, &post(&format!("/run/{id}"), b"{not json"));
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn run_empty_body_is_400() {
        let (_dir, mut state, id) = setup(None);
        let resp = handle(&mut state, &post(&format!("/run/{id}"), b""));
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn unknown_route_is_404() {
        let (_dir, mut state, _id) = setup(None);
        let resp = handle(&mut state, &get("/nope"));
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn wrong_method_on_health_is_405() {
        let (_dir, mut state, _id) = setup(None);
        let resp = handle(&mut state, &post("/health", b""));
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn wrong_method_on_artifacts_is_405() {
        let (_dir, mut state, _id) = setup(None);
        let resp = handle(&mut state, &post("/artifacts", b""));
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn wrong_method_on_run_is_405() {
        let (_dir, mut state, id) = setup(None);
        let resp = handle(&mut state, &get(&format!("/run/{id}")));
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn cache_serves_a_second_run() {
        let (_dir, mut state, id) = setup(None);
        let first = handle(&mut state, &post(&format!("/run/{id}"), br#"[1,2]"#));
        let second = handle(&mut state, &post(&format!("/run/{id}"), br#"[3,4]"#));
        assert_eq!(first.status, 200);
        assert_eq!(second.status, 200);
        assert_eq!(second.body, json!({ "output": [3, 4] }));
        assert!(state.cache.contains_key(&id), "artifact cached after run");
    }

    // ---- capability artifacts (ADR-0017 amendment, wave 7) ----

    #[test]
    fn run_capability_artifact_without_a_table_is_a_load_problem() {
        // a pure server (no --tool) cannot cover the declared capability; the
        // loader refuses and it surfaces honestly as a 500 naming the tool.
        let (_dir, mut state, id) = setup_tool(None);
        let resp = handle(&mut state, &post(&format!("/run/{id}"), br#"{"q":"beta"}"#));
        assert_eq!(resp.status, 500);
        let msg = resp.body["error"].as_str().expect("error string");
        assert!(msg.contains("no tool host"), "{msg}");
        assert!(msg.contains("lookup"), "{msg}");
    }

    #[test]
    fn run_capability_artifact_with_the_table_answers() {
        // the server table covers `lookup`; /run answers end to end
        // (input -> tool -> output), the true tier-1 capability path.
        let input = json!({ "q": "beta" });
        let (_dir, mut state, id) = setup_tool(Some(lookup_replay(&input, json!("TEAM-B"))));
        let resp = handle(
            &mut state,
            &post(&format!("/run/{id}"), input.to_string().as_bytes()),
        );
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, json!({ "output": "TEAM-B" }));
    }

    #[test]
    fn pure_artifact_is_unaffected_when_the_server_has_a_table() {
        // a server WITH a table still serves a PURE artifact: it loads with no
        // host (the loader refuses a host on a pure artifact), so pure
        // artifacts are completely unaffected.
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry")).unwrap();
        let src = dir.path().join("echo.cbin");
        std::fs::write(&src, artifact_bytes(None)).unwrap();
        let id = registry.add(&src, false).unwrap().id;
        let table = lookup_replay(&json!("unused"), json!("unused"));
        let mut state = ServerState::with_tools(registry, Some(table));
        let resp = handle(&mut state, &post(&format!("/run/{id}"), br#"{"a":1}"#));
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, json!({ "output": { "a": 1 } }));
    }

    #[test]
    fn parse_tool_table_empty_is_no_host() {
        assert!(parse_tool_table(&[]).unwrap().is_none());
    }

    #[test]
    fn parse_tool_table_builds_a_live_table() {
        let table = parse_tool_table(&["lookup=py tool.py".to_owned()])
            .unwrap()
            .expect("a flag builds a table");
        match table {
            HostTools::Live(map) => {
                assert_eq!(
                    map.get("lookup"),
                    Some(&vec!["py".to_owned(), "tool.py".to_owned()])
                );
            }
            HostTools::Replay(_) => panic!("--tool builds a Live table"),
            HostTools::Callback { .. } => panic!("--tool builds a Live table"),
        }
    }

    #[test]
    fn parse_tool_table_rejects_a_malformed_flag() {
        for bad in ["noequals", "=command", "name="] {
            match parse_tool_table(&[bad.to_owned()]) {
                Err(ServeError::Config { .. }) => {}
                other => panic!("{bad:?} must be a Config error, got {other:?}"),
            }
        }
    }

    // ---- per-request tool budget + audit (ADR-0028) ----

    /// A fake inner tool execution for budget tests: counts its own runs and
    /// always answers, so the budget/audit logic is exercised with no process
    /// spawned (the subprocess path is `spawn_tool`, mirrored from the runtime
    /// and covered by loopback e2e).
    fn counting_inner(
        runs: Arc<AtomicU64>,
    ) -> impl FnMut(&str, &Value) -> Result<Value, String> + Send + 'static {
        move |_name: &str, _input: &Value| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(json!("TEAM-B"))
        }
    }

    #[test]
    fn budgeted_host_allows_up_to_budget_then_refuses() {
        let counter = Arc::new(AtomicU64::new(0));
        let inner_runs = Arc::new(AtomicU64::new(0));
        let host = budgeted(
            std::iter::once("lookup".to_owned()).collect(),
            2,
            Arc::clone(&counter),
            counting_inner(Arc::clone(&inner_runs)),
        );
        let HostTools::Callback { call, .. } = &host else {
            panic!("budgeted builds a Callback host");
        };
        // invoke the wrapper's dispatch closure directly (the seam locks the
        // same mutex per call) — no wasm needed to prove the budget logic.
        let invoke = || {
            let mut guard = call.lock().expect("lock");
            guard("lookup", &json!({ "q": 1 }))
        };
        assert_eq!(invoke().expect("call 1"), json!("TEAM-B"));
        assert_eq!(invoke().expect("call 2"), json!("TEAM-B"));
        let over = invoke().expect_err("call 3 exceeds budget 2");
        assert_eq!(over, "tool budget exceeded: 2 per request (ADR-0028)");
        assert_eq!(
            inner_runs.load(Ordering::Relaxed),
            2,
            "the tool ran only for the two allowed calls"
        );
        assert_eq!(
            counter.load(Ordering::Relaxed),
            3,
            "the counter also counts the refused attempt"
        );
    }

    #[test]
    fn tool_budget_resets_per_request() {
        // budget 1, artifact calls lookup once per run: two back-to-back
        // requests both succeed ONLY if the counter resets between them.
        let (_dir, registry, id) = tool_registry(1);
        let counter = Arc::new(AtomicU64::new(0));
        let inner_runs = Arc::new(AtomicU64::new(0));
        let host = budgeted(
            std::iter::once("lookup".to_owned()).collect(),
            1,
            Arc::clone(&counter),
            counting_inner(Arc::clone(&inner_runs)),
        );
        let mut state = ServerState {
            registry,
            tools: Some(host),
            tool_calls: counter,
            cache: BTreeMap::new(),
        };
        let body = br#"{"q":"beta"}"#;
        let first = handle(&mut state, &post(&format!("/run/{id}"), body));
        let second = handle(&mut state, &post(&format!("/run/{id}"), body));
        assert_eq!(first.status, 200, "first: {:?}", first.body);
        assert_eq!(
            second.status, 200,
            "second (after per-request reset): {:?}",
            second.body
        );
        assert_eq!(second.body, json!({ "output": "TEAM-B" }));
        assert_eq!(
            inner_runs.load(Ordering::Relaxed),
            2,
            "the tool ran once per request"
        );
    }

    #[test]
    fn tool_budget_refuses_over_budget_call_within_one_request() {
        // budget 1, artifact calls lookup TWICE in one request: the 2nd call is
        // refused, the artifact traps, the request is a 500 (not an answer), and
        // the tool ran exactly once — unbounded execution is stopped.
        let (_dir, registry, id) = tool_registry(2);
        let counter = Arc::new(AtomicU64::new(0));
        let inner_runs = Arc::new(AtomicU64::new(0));
        let host = budgeted(
            std::iter::once("lookup".to_owned()).collect(),
            1,
            Arc::clone(&counter),
            counting_inner(Arc::clone(&inner_runs)),
        );
        let mut state = ServerState {
            registry,
            tools: Some(host),
            tool_calls: counter,
            cache: BTreeMap::new(),
        };
        let resp = handle(&mut state, &post(&format!("/run/{id}"), br#"{"q":"beta"}"#));
        assert_eq!(
            resp.status, 500,
            "over-budget request must fail, not answer: {:?}",
            resp.body
        );
        assert_eq!(
            inner_runs.load(Ordering::Relaxed),
            1,
            "the tool ran only for the one allowed call"
        );
    }
}
