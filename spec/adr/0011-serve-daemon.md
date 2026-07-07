# ADR-0011: `auto serve` — the tier-1 runtime as a long-running process

status: accepted · scope: `crates/auto-serve` (new build-out over the frozen seam), `spec/runtime.md` §8

## context

`auto run` executes one artifact on one input and exits. The end state
(CLAUDE.md) is a **planetary tiered runtime** where nothing is figured out
twice — which needs the runtime as a *process*: a registry of compiled
cognition binaries, served over the network, answering requests on the
compiled fast path. This ADR is the v0 floor of that: a single-node,
**read-only, tier-1-only** artifact server. It serves what the registry
already holds; it does not compile, sign, record, or recompile.

The frozen seam (`ServeConfig`, `ServeError`, committed earlier this wave)
fixed the config and error surface; this ADR records the build-out decisions.

## decision

1. **Registry is the artifact source; content is re-verified on load.** The
   server loads artifacts through `Registry::get`, which re-parses the stored
   bytes, recomputes the content id, and verifies any detached signature
   (ADR-0008) before handing them over. A tamper mismatch is a 500, never a
   silently served artifact. `get` writes verified bytes to a path, so the
   load path receives them in a unique temp file and reads them back — the
   same temp-file pattern `auto-cli` uses, keeping the runtime dependency set
   exactly as frozen (`tempfile` stays a dev-only dep).

2. **Abstain, do not deopt (v0).** A guarded artifact evaluates its guard
   first, exactly as `auto run`: **proceed** runs tier-1; **trip** returns
   `409 {"abstained":true,...}` with the guard's reason/distance/threshold.
   There is **no in-server tier-0**. A server-side deopt would spend on a
   frontier model per inbound request, and no per-request spend policy exists
   — who authorizes it, against which session cap, charged to whom (ADR-0010
   caps are per-CLI-session). Serving a compiled answer beyond calibration is
   the silent correctness failure guards exist to prevent, so the server
   abstains rather than guess. This mirrors `auto run` without `--tier0`
   (which exits 3); over HTTP the honest analogue is 409 Conflict — the
   request cannot be satisfied on the only tier the server offers.

3. **Pure handler, thin socket shell.** `api::handle(&mut ServerState,
   &ApiRequest) -> ApiResponse` is a total function over parsed structs and
   holds all routing and the guard-then-execute decision; `server.rs` only
   maps `tiny_http` requests into `ApiRequest` and writes `ApiResponse` back.
   Every route is therefore unit-tested with a real registry and a real
   runnable (wat-compiled ECHO) artifact and **no socket** (CLAUDE.md: no
   network in tests; loopback e2e is the orchestrator's job).

4. **Blocking `tiny_http`, sequential loop (v0).** One request at a time:
   `recv` → parse → `handle` → `respond`, one log line each. No tokio, matching
   the synchronous-workspace rationale of ADR-0010. Correctness first;
   thread-per-request concurrency is a recorded upgrade, not a v0 feature. A
   socket-level `recv` failure ends the loop as `ServeError::Loop` (honest;
   accept-retry resilience is future work).

5. **Load-once cache keyed by content id.** The parsed guard and compiled
   `WasmExecutor` are cached per id on first load; subsequent runs reuse the
   compiled module. `WasmExecutor::execute` already instantiates a fresh
   store+instance per call (executor.rs), so caching preserves `auto run`'s
   per-call isolation while paying compilation once. The cache is sound
   because ids are content addresses — an id pins its bytes, so a cached
   executor cannot go stale. It is **not evicted**: a deleted/replaced
   artifact keeps answering from cache until restart (no hot reload in v0).

6. **v0 does not conformance-check the input.** `auto run` checks the input
   against the manifest's declared type via `auto-contract`; auto-serve
   deliberately does not depend on `auto-contract`, so it omits that check. A
   guarded artifact trips on a wrong-shaped input anyway; an unguarded one
   surfaces the module's own output or a tier-1 failure (500). Recorded as an
   upgrade, not hidden.

## alternatives considered

**axum / tokio / hyper.** The ecosystem default; buys async concurrency and a
router. Rejected for v0 for the same reason as ADR-0010: it drags an async
runtime into a synchronous workspace for a server that answers a handful of
sequential requests. `tiny_http` is a blocking accept loop with no runtime;
the pure-handler seam makes the transport swappable later without touching
routing or semantics.

**In-server tier-0 (deopt over HTTP).** Would make the server a full ratchet
node. Rejected: needs the per-request spend policy that does not exist
(decision 2). Deferred until an authorization/accounting model is designed.

**Hot artifact reload / cache eviction / a watch on the registry.** Would let
a running server pick up newly added or removed artifacts without restart.
Rejected for v0: content-addressed ids make the cache correct-by-construction
for what it holds; liveness of *removals* and eviction policy are additive and
unneeded for a read-only demo server.

**Recording proxy semantics (record inbound traffic).** That is `auto-proxy`'s
job (a sibling crate); auto-serve serves compiled artifacts and does not
record. Kept separate.

## consequences

- A registry of `.cbin`s can be served over HTTP with the same guard-gated
  correctness as `auto run`, and every request re-verifies content integrity
  through `Registry::get`.
- The server never spends and never emits: no key, no frontier calls, no
  artifact writes. The only failure that stops it is a socket accept error.
- Concurrency, in-server deopt, hot reload, TLS, and auth are all recorded
  upgrades — stated in `spec/runtime.md` §8, not papered over.

## sources

- `tiny_http` 0.12.0 API — `Server::http(addr) -> Result<Server, _>`,
  `Server::recv() -> IoResult<Request>`, `Request::method() -> &Method`
  (variants `Get`/`Post`/…, PascalCase), `Request::url() -> &str`,
  `Request::as_reader() -> &mut dyn Read`, `Request::respond(Response<R>)`,
  `Response::from_data(D: Into<Vec<u8>>) -> Response<Cursor<Vec<u8>>>`,
  `.with_status_code(S: Into<StatusCode>)`, `.with_header(H: Into<Header>)`,
  `Header::from_bytes(B1, B2) -> Result<Header, ()>`:
  <https://docs.rs/tiny_http/0.12.0/tiny_http/>
