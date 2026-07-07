//! `auto` — toolchain cli.
//!
//! Every subcommand is real, and every limit is disclosed in the help text
//! itself: synthesis is enumerative (not LLM-guided), distillation is a
//! decision tree, guards are trigram-Jaccard (not embeddings), tier-0 is a
//! pluggable command (not yet a frontier model).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "auto",
    version,
    about = "Auto — the cognition compiler toolchain.\n\
             record | report | verify | compile (hand-assisted or synthesized) | distill | \
             run (guarded tier-1, deopt to --tier0) | inspect"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// run a command instrumented with an auto SDK and ingest its trace
    Record {
        /// trace store to ingest into (created if missing)
        #[arg(long, default_value = "auto-traces.db")]
        store: PathBuf,
        /// keep the raw JSONL trace at this path instead of a temp file
        #[arg(long)]
        keep_jsonl: Option<PathBuf>,
        /// recover a torn FINAL line (hard-kill mid-write): parse until the
        /// tear, ingest the trace marked PARTIAL (excluded from witnessing;
        /// ADR-0030). Default = strict: any tear fails the whole file
        #[arg(long)]
        recover_partial: bool,
        /// the command to run; AUTO_TRACE_FILE is set for it
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// print the determinism report for a task (measured, never extrapolated)
    Report {
        /// task label to analyze (as recorded by the SDK)
        #[arg(long)]
        task: String,
        /// trace store to read
        #[arg(long, default_value = "auto-traces.db")]
        store: PathBuf,
    },
    /// verify a contract against recorded traces; writes a content-addressed eval run.
    /// exit codes: 0 = PASS, 1 = FAIL (or error), 2 = INCONCLUSIVE
    Verify {
        /// path to a contract file (*.contract.toml)
        #[arg(long)]
        contract: PathBuf,
        /// trace store to verify against
        #[arg(long, default_value = "auto-traces.db")]
        store: PathBuf,
        /// directory for eval-run records
        #[arg(long, default_value = "evals/runs")]
        runs_dir: PathBuf,
    },
    /// compile a span contract into a .cbin artifact. Without --module the
    /// implementation is SYNTHESIZED by enumerative search over recorded
    /// observations (S4; not LLM-guided yet); with --module it is
    /// hand-supplied. Either way, emit happens only if contract verification
    /// + differential replay of every distinct recorded input reach PASS
    Compile {
        /// path to a span-scope contract (*.contract.toml)
        #[arg(long)]
        contract: PathBuf,
        /// trace store holding the recorded reference runs
        #[arg(long, default_value = "auto-traces.db")]
        store: PathBuf,
        /// hand-supplied wasm module (frozen ABI, zero imports); omit to
        /// synthesize the implementation from the recorded observations
        #[arg(long)]
        module: Option<PathBuf>,
        /// synthesis mode without --module: "enum" (enumerative search) or
        /// "llm" (frontier-proposed CEGIS under the spend cap; ADR-0010 —
        /// needs OPENAI_API_KEY in the environment or ./.env, and a nonzero
        /// --spend-cap-usd)
        #[arg(long, default_value = "enum")]
        synth: String,
        /// frontier model for --synth llm (must be in the pinned price table)
        #[arg(long, default_value = "gpt-5.4-mini")]
        frontier_model: String,
        /// session spend cap in USD (exact decimal, e.g. "1" or "0.25");
        /// 0 — the default — refuses every paid call (fail-closed)
        #[arg(long, default_value = "0")]
        spend_cap_usd: String,
        /// spend-ledger session the cap applies to
        #[arg(long, default_value = "default")]
        session: String,
        /// input object field holding text: build a runtime guard from the
        /// witnessed inputs (omit = unguarded artifact)
        #[arg(long)]
        guard_field: Option<String>,
        /// guard threshold calibration: split-conformal miscoverage alpha in
        /// thousandths (ADR-0014); the default 1 is the v0-equivalent max
        /// quantile
        #[arg(long, default_value_t = 1)]
        guard_alpha_milli: u32,
        /// upgrade the guard wire to v2: dense lexical trigram-hash
        /// embeddings with cosine OOD distance, split-conformally calibrated
        /// (ADR-0023). Lexical, not semantic — a disjoint-vocabulary
        /// paraphrase still trips
        #[arg(long, default_value_t = false, requires = "guard_field")]
        guard_embedding: bool,
        /// divergent recorded references: "refuse" (default) or
        /// "most-common" — train on the majority witness (ties break to the
        /// lexicographically smaller canonical output; ADR-0018 amendment).
        /// The declared acceptance threshold still decides the gate verdict
        #[arg(long, default_value = "refuse")]
        divergent_pick: String,
        /// judge for match="judged" examples: a spend-capped frontier model
        /// (needs a nonzero --spend-cap-usd; every call ledgered as "judge").
        /// Omitted = judged examples verdict Inconclusive (ADR-0019)
        #[arg(long)]
        judge_model: Option<String>,
        /// output artifact path (conventionally *.cbin)
        #[arg(long)]
        out: PathBuf,
        /// directory for eval-run records
        #[arg(long, default_value = "evals/runs")]
        runs_dir: PathBuf,
    },
    /// distill a span contract into a .cbin artifact: an external trainer
    /// fits a small specialist (v0: a decision tree over char-trigram
    /// features) to the recorded observations, then emission is gated on the
    /// SAME contract + differential PASS as compile — holdout metrics are
    /// provenance, never a substitute for the gate
    Distill {
        /// path to a span-scope contract (*.contract.toml)
        #[arg(long)]
        contract: PathBuf,
        /// trace store holding the recorded reference runs
        #[arg(long, default_value = "auto-traces.db")]
        store: PathBuf,
        /// trainer command, split on spaces (e.g. "python crates/auto-passes/trainer/tree_train.py")
        #[arg(long)]
        trainer: String,
        /// model kind the trainer emits: "tree" (sklearn decision tree) or
        /// "mlp" (torch, plain-weights json; train on Modal for GPU)
        #[arg(long, default_value = "tree")]
        model_kind: String,
        /// input object field holding the text (omit when the input IS text)
        #[arg(long)]
        input_field: Option<String>,
        /// fraction of observations the trainer holds out
        #[arg(long, default_value_t = 0.25)]
        holdout: f64,
        /// trainer seed (determinism)
        #[arg(long, default_value_t = 0)]
        seed: u64,
        /// the trainer refuses below this holdout accuracy
        #[arg(long, default_value_t = 1.0)]
        min_holdout_accuracy: f64,
        /// guard threshold calibration: split-conformal miscoverage alpha in
        /// thousandths (ADR-0014); the default 1 is the v0-equivalent max
        /// quantile
        #[arg(long, default_value_t = 1)]
        guard_alpha_milli: u32,
        /// upgrade the guard wire to v2: dense lexical trigram-hash
        /// embeddings with cosine OOD distance, split-conformally calibrated
        /// (ADR-0023). Lexical, not semantic
        #[arg(long, default_value_t = false, requires = "input_field")]
        guard_embedding: bool,
        /// divergent recorded references: "refuse" (default), "most-common"
        /// — train on the majority witness (ADR-0018 amendment) — or
        /// "weighted" — train on EVERY witnessed output, weighted by its
        /// witness count (ADR-0031). Either selects training data only; the
        /// declared acceptance threshold still decides the gate verdict
        #[arg(long, default_value = "refuse")]
        divergent_pick: String,
        /// judge for match="judged" examples (spend-capped; ADR-0019)
        #[arg(long)]
        judge_model: Option<String>,
        /// session spend cap in USD for the judge (exact decimal); 0 — the
        /// default — refuses every paid call (fail-closed)
        #[arg(long, default_value = "0")]
        spend_cap_usd: String,
        /// spend-ledger session the cap applies to
        #[arg(long, default_value = "default")]
        session: String,
        /// output artifact path (conventionally *.cbin)
        #[arg(long)]
        out: PathBuf,
        /// directory for eval-run records
        #[arg(long, default_value = "evals/runs")]
        runs_dir: PathBuf,
    },
    /// execute a .cbin artifact on one input. Guarded artifacts evaluate the
    /// guard first: in-distribution runs tier-1; a tripped guard DEOPTS to
    /// --tier0 (recording the observation into --store for recompilation —
    /// the ratchet) or ABSTAINS with exit 3 when no tier-0 is configured
    Run {
        /// artifact to execute
        #[arg(long)]
        artifact: PathBuf,
        /// input value as JSON text; must conform to the artifact's declared
        /// input type (required unless --stdio)
        #[arg(long)]
        input: Option<String>,
        /// resident stdio mode: one JSON value per line on stdin, one JSON
        /// object per line on stdout, until EOF. The module compiles ONCE
        /// and is reused (kills per-call spawn + compile overhead); guard
        /// trips ABSTAIN per line (no tier-0 here — spec/runtime.md §9)
        #[arg(long, conflicts_with = "input")]
        stdio: bool,
        /// tier-0 fallback: a command (split on spaces; receives the
        /// canonical input JSON as its final argument, prints the output
        /// JSON) or "frontier:<model-id>" — a spend-capped frontier model as
        /// the interpreter (needs OPENAI_API_KEY in the environment or
        /// ./.env, and a nonzero --spend-cap-usd; spec/runtime.md §3)
        #[arg(long)]
        tier0: Option<String>,
        /// live tool for a capability artifact, as name=command (repeatable;
        /// the command gets the canonical input JSON as its final argument
        /// and prints the output JSON — the tier-0 command contract). Every
        /// declared capability needs one, or the load refuses (ADR-0017)
        #[arg(long = "tool")]
        tools: Vec<String>,
        /// session spend cap in USD for frontier tier-0 (exact decimal);
        /// 0 — the default — refuses every paid call (fail-closed)
        #[arg(long, default_value = "0")]
        spend_cap_usd: String,
        /// spend-ledger session the cap applies to
        #[arg(long, default_value = "default")]
        session: String,
        /// trace store to ingest deopt observations into (the ratchet)
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// pretty-print a serialized IR graph (*.air) or a .cbin artifact manifest
    Inspect {
        /// path to an IR graph (AIR0) or artifact (ACB0)
        file: PathBuf,
    },
    /// serve registry artifacts over HTTP: guard-gated tier-1 per request,
    /// honest 409 abstention on a guard trip (no in-server tier-0 in v0 —
    /// per-request deopt spend policy is an unresolved design; spec/runtime.md §8)
    Serve {
        /// registry root (default: AUTO_REGISTRY env or ~/.auto/registry)
        #[arg(long)]
        registry: Option<PathBuf>,
        /// bind address
        #[arg(long, default_value = "127.0.0.1:7433")]
        addr: String,
        /// server-level tool table for capability artifacts, name=command
        /// (repeatable; the operator chooses the table, requesters cannot —
        /// ADR-0017 amendment)
        #[arg(long = "tool")]
        tools: Vec<String>,
        /// max tool calls one request may trigger before the server refuses
        /// further tool execution (ADR-0028); absent = unlimited (today's
        /// behavior)
        #[arg(long)]
        max_tool_calls_per_request: Option<u64>,
    },
    /// record any OpenAI-backed agent with ZERO code changes: an
    /// api-compatible endpoint that forwards each call (with the caller's
    /// own Authorization header — the proxy holds no key) to the upstream
    /// and ingests the exchange as a trace with measured cost/token attrs
    Proxy {
        /// upstream base URL
        #[arg(long, default_value = "https://api.openai.com")]
        upstream: String,
        /// trace store to ingest recorded exchanges into
        #[arg(long)]
        store: PathBuf,
        /// bind address
        #[arg(long, default_value = "127.0.0.1:7434")]
        addr: String,
        /// task name recorded on every ingested trace
        #[arg(long, default_value = "proxied-agent")]
        task: String,
    },
    /// the ratchet as a service: watch a trace store and, when deopt-ingested
    /// evidence grows, run the recompile command and publish the emitted
    /// artifact to the registry — the gate stays exactly the gate (ADR-0013)
    Daemon {
        /// trace store to watch (the deopt ingestion target)
        #[arg(long)]
        store: PathBuf,
        /// contract whose scope defines the watched distinct-input count
        #[arg(long)]
        contract: PathBuf,
        /// registry to publish recompiled artifacts to (default: AUTO_REGISTRY
        /// env or ~/.auto/registry)
        #[arg(long)]
        registry: Option<PathBuf>,
        /// recompile command, split on spaces; must contain the placeholder
        /// {out}, replaced with the artifact path the daemon expects written
        #[arg(long)]
        recompile: String,
        /// poll interval between store checks, milliseconds
        #[arg(long, default_value_t = 5000)]
        poll_interval_ms: u64,
        /// run exactly one check-and-maybe-recompile cycle, then exit
        #[arg(long)]
        once: bool,
        /// persistent watermark file; the last-compiled count survives a
        /// restart (default: in-memory only — one redundant recompile after
        /// a restart)
        #[arg(long)]
        watermark: Option<PathBuf>,
        /// supervised mode: retry a retryable cycle failure after exponential
        /// backoff instead of exiting (config-shaped errors still exit loudly)
        #[arg(long)]
        supervise: bool,
    },
    /// prune eval-run records: keep the newest --keep plus every run pinned
    /// by a registry manifest (ADR-0020). Always deletes; there is no dry-run
    RunsGc {
        /// directory of eval-run records
        #[arg(long, default_value = "evals/runs")]
        runs_dir: PathBuf,
        /// always retain the newest N records (a floor; mtime ties keep more)
        #[arg(long)]
        keep: usize,
        /// also require records be strictly older than this many days before
        /// deletion (age RESTRICTS deletion — only records already past
        /// --keep are eligible, pins always kept; ADR-0020 age amendment)
        #[arg(long)]
        max_age_days: Option<u64>,
        /// byte ceiling for retained records: past the keep floor and pins,
        /// oldest eligible records are removed until under the ceiling; pins
        /// and floor survive an exceeded ceiling LOUDLY (ADR-0020 2nd
        /// amendment)
        #[arg(long)]
        max_total_bytes: Option<u64>,
        /// registry root whose manifests pin protected runs
        /// (default: AUTO_REGISTRY env, else ~/.auto/registry)
        #[arg(long)]
        registry: Option<PathBuf>,
    },
    /// local content-addressed artifact registry with detached ed25519
    /// signatures (v0: single local keypair; sigstore is the recorded target)
    Registry {
        #[command(subcommand)]
        cmd: RegistryCmd,
    },
}

#[derive(Subcommand)]
enum RegistryCmd {
    /// generate the registry keypair (refuses to overwrite existing keys)
    Keygen {
        /// registry root (default: AUTO_REGISTRY env, else ~/.auto/registry)
        #[arg(long)]
        registry: Option<PathBuf>,
    },
    /// validate and store an artifact by content id; --sign writes a
    /// detached signature (signing never changes the id)
    Add {
        /// artifact to add (*.cbin)
        artifact: PathBuf,
        /// sign the artifact bytes with the registry key
        #[arg(long)]
        sign: bool,
        #[arg(long)]
        registry: Option<PathBuf>,
    },
    /// list stored artifacts with task, scope, and signature status
    List {
        #[arg(long)]
        registry: Option<PathBuf>,
    },
    /// copy an artifact out — content id recomputed and signature verified
    /// before anything is served (tamper evidence)
    Get {
        /// 64-hex content id
        id: String,
        /// destination path
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        registry: Option<PathBuf>,
    },
    /// serve this registry root over loopback HTTP so peers can push/pull
    /// (ADR-0022; NO auth, NO TLS — a development transport)
    Serve {
        #[arg(long)]
        registry: Option<PathBuf>,
        /// bind address
        #[arg(long, default_value = "127.0.0.1:7435")]
        addr: String,
    },
    /// push an artifact (and its detached signature, if signed) to a remote
    /// registry by content id
    Push {
        /// 64-hex content id
        id: String,
        /// remote base URL, e.g. http://host:7435
        #[arg(long)]
        remote: String,
        #[arg(long)]
        registry: Option<PathBuf>,
    },
    /// pull an artifact from a remote registry into this root — content id
    /// recomputed and signature verified before anything is written
    Pull {
        /// 64-hex content id
        id: String,
        #[arg(long)]
        remote: String,
        #[arg(long)]
        registry: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().cmd {
        Cmd::Record {
            store,
            keep_jsonl,
            recover_partial,
            command,
        } => record(&store, keep_jsonl.as_deref(), recover_partial, &command),
        Cmd::Report { task, store } => report(&task, &store),
        Cmd::Verify {
            contract,
            store,
            runs_dir,
        } => verify(&contract, &store, &runs_dir),
        Cmd::Compile {
            contract,
            store,
            module,
            synth,
            frontier_model,
            spend_cap_usd,
            session,
            guard_field,
            guard_alpha_milli,
            guard_embedding,
            divergent_pick,
            judge_model,
            out,
            runs_dir,
        } => compile(
            &contract,
            &store,
            module.as_deref(),
            &synth,
            &frontier_model,
            &spend_cap_usd,
            &session,
            guard_field.as_deref(),
            guard_alpha_milli,
            guard_embedding,
            &divergent_pick,
            judge_model.as_deref(),
            &out,
            &runs_dir,
        ),
        Cmd::Distill {
            contract,
            store,
            trainer,
            model_kind,
            input_field,
            holdout,
            seed,
            min_holdout_accuracy,
            guard_alpha_milli,
            guard_embedding,
            divergent_pick,
            judge_model,
            spend_cap_usd,
            session,
            out,
            runs_dir,
        } => distill(
            &contract,
            &store,
            &trainer,
            &model_kind,
            input_field.as_deref(),
            holdout,
            seed,
            min_holdout_accuracy,
            guard_alpha_milli,
            guard_embedding,
            &divergent_pick,
            judge_model.as_deref(),
            &spend_cap_usd,
            &session,
            &out,
            &runs_dir,
        ),
        Cmd::Run {
            artifact,
            input,
            stdio,
            tier0,
            tools,
            spend_cap_usd,
            session,
            store,
        } => {
            if stdio {
                return run_stdio(&artifact, &tools);
            }
            let Some(input) = input else {
                eprintln!("auto run: --input is required unless --stdio");
                return ExitCode::FAILURE;
            };
            run_artifact(
                &artifact,
                &input,
                tier0.as_deref(),
                &tools,
                &spend_cap_usd,
                &session,
                store.as_deref(),
            )
        }
        Cmd::Inspect { file } => inspect(&file),
        Cmd::Serve {
            registry,
            addr,
            tools,
            max_tool_calls_per_request,
        } => {
            let root = match registry_root(registry) {
                Ok(r) => r,
                Err(code) => return code,
            };
            match auto_serve::serve(auto_serve::ServeConfig {
                registry_root: root,
                addr,
                tools,
                max_tool_calls_per_request,
            }) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("auto serve: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::Proxy {
            upstream,
            store,
            addr,
            task,
        } => match auto_proxy::proxy(auto_proxy::ProxyConfig {
            upstream,
            store,
            addr,
            task,
        }) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("auto proxy: {e}");
                ExitCode::FAILURE
            }
        },
        Cmd::Daemon {
            store,
            contract,
            registry,
            recompile,
            poll_interval_ms,
            once,
            watermark,
            supervise,
        } => {
            let root = match registry_root(registry) {
                Ok(r) => r,
                Err(code) => return code,
            };
            let recompile_argv: Vec<String> =
                recompile.split_whitespace().map(str::to_owned).collect();
            match auto_daemon::daemon(auto_daemon::DaemonConfig {
                store,
                contract,
                registry_root: root,
                recompile_argv,
                poll_interval_ms,
                once,
                watermark_path: watermark,
                supervise,
            }) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("auto daemon: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::RunsGc {
            runs_dir,
            keep,
            max_age_days,
            max_total_bytes,
            registry,
        } => {
            let root = match registry_root(registry) {
                Ok(r) => r,
                Err(code) => return code,
            };
            let reg = match auto_registry::Registry::open(&root) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("auto runs-gc: open registry {}: {e}", root.display());
                    return ExitCode::FAILURE;
                }
            };
            // protection set FIRST: a registry we cannot fully read BLOCKS the
            // sweep — never delete a run a corrupt-but-real manifest might pin
            let protected = match reg.pinned_eval_runs() {
                Ok(set) => set,
                Err(e) => {
                    eprintln!(
                        "auto runs-gc: cannot build protected set from {}: {e}; \
                         refusing to prune",
                        root.display()
                    );
                    return ExitCode::FAILURE;
                }
            };
            // an absurd horizon saturates to the epoch (a no-op), never panics
            let older_than = max_age_days.map(|days| {
                let horizon = std::time::Duration::from_secs(days.saturating_mul(86_400));
                std::time::SystemTime::now()
                    .checked_sub(horizon)
                    .unwrap_or(std::time::UNIX_EPOCH)
            });
            match auto_contract::evalrun::gc_with_limits(
                &runs_dir,
                keep,
                &protected,
                older_than,
                max_total_bytes,
            ) {
                Ok(out) => {
                    let r = &out.report;
                    print!(
                        "runs-gc {}: removed {} kept {} protected-kept {} kept-bytes {} (protected set {})",
                        runs_dir.display(),
                        r.removed,
                        r.kept,
                        r.protected_kept,
                        out.kept_bytes,
                        protected.len()
                    );
                    if out.over_ceiling() {
                        print!(
                            " — OVER CEILING: {} bytes retained exceeds {} (only floor/pinned records remain)",
                            out.kept_bytes,
                            max_total_bytes.expect("over_ceiling implies a ceiling")
                        );
                    }
                    println!();
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("auto runs-gc: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::Registry { cmd } => registry_cmd(cmd),
    }
}

fn registry_root(flag: Option<PathBuf>) -> Result<PathBuf, ExitCode> {
    if let Some(root) = flag {
        return Ok(root);
    }
    if let Ok(env_root) = std::env::var("AUTO_REGISTRY") {
        return Ok(PathBuf::from(env_root));
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| {
            eprintln!(
                "auto registry: no --registry, no AUTO_REGISTRY, and no home \
                 directory to default to"
            );
            ExitCode::FAILURE
        })?;
    Ok(PathBuf::from(home).join(".auto").join("registry"))
}

fn registry_cmd(cmd: RegistryCmd) -> ExitCode {
    match cmd {
        RegistryCmd::Keygen { registry } => {
            let root = match registry_root(registry) {
                Ok(r) => r,
                Err(code) => return code,
            };
            let reg = match auto_registry::Registry::open(&root) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("auto registry keygen: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match reg.keygen() {
                Ok(pub_path) => {
                    println!("keys written: {}", pub_path.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("auto registry keygen: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        RegistryCmd::Add {
            artifact,
            sign,
            registry,
        } => {
            let root = match registry_root(registry) {
                Ok(r) => r,
                Err(code) => return code,
            };
            let reg = match auto_registry::Registry::open(&root) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("auto registry add: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match reg.add(&artifact, sign) {
                Ok(outcome) => {
                    println!(
                        "artifact {} added ({}) -> {}",
                        outcome.id,
                        if outcome.signed { "signed" } else { "unsigned" },
                        outcome.stored_at.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("auto registry add: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        RegistryCmd::List { registry } => {
            let root = match registry_root(registry) {
                Ok(r) => r,
                Err(code) => return code,
            };
            let reg = match auto_registry::Registry::open(&root) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("auto registry list: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match reg.list() {
                Ok(entries) => {
                    if entries.is_empty() {
                        println!("(registry is empty)");
                    }
                    for entry in entries {
                        if let Some(problem) = &entry.problem {
                            println!("{} PROBLEM: {problem}", entry.id);
                            continue;
                        }
                        let signature = match (entry.signed, entry.verified) {
                            (false, _) => "unsigned".to_owned(),
                            (true, Some(true)) => "signed verified=true".to_owned(),
                            (true, Some(false)) => "signed verified=FALSE".to_owned(),
                            (true, None) => "signed verified=unknown (no public key)".to_owned(),
                        };
                        println!(
                            "{} task \"{}\" {} eval-runs={} {signature}",
                            entry.id, entry.task, entry.scope, entry.eval_runs
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("auto registry list: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        RegistryCmd::Serve { registry, addr } => {
            let root = match registry_root(registry) {
                Ok(r) => r,
                Err(code) => return code,
            };
            match auto_registry::remote::serve_addr(&root, &addr) {
                Ok(()) => ExitCode::SUCCESS, // returns only on a socket-level failure
                Err(e) => {
                    eprintln!("auto registry serve: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        RegistryCmd::Push {
            id,
            remote,
            registry,
        } => {
            let root = match registry_root(registry) {
                Ok(r) => r,
                Err(code) => return code,
            };
            match auto_registry::remote::push(&remote, &root, &id) {
                Ok(s) => {
                    println!(
                        "pushed {} -> {remote} ({}{})",
                        s.id,
                        if s.created {
                            "created"
                        } else {
                            "already present"
                        },
                        if s.signed { ", signature accepted" } else { "" },
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("auto registry push: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        RegistryCmd::Pull {
            id,
            remote,
            registry,
        } => {
            let root = match registry_root(registry) {
                Ok(r) => r,
                Err(code) => return code,
            };
            match auto_registry::remote::pull(&remote, &root, &id) {
                Ok(s) => {
                    println!(
                        "pulled {} <- {remote} (content verified{})",
                        s.id,
                        if s.signed {
                            ", signature verified"
                        } else {
                            ", unsigned"
                        },
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("auto registry pull: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        RegistryCmd::Get { id, out, registry } => {
            let root = match registry_root(registry) {
                Ok(r) => r,
                Err(code) => return code,
            };
            let reg = match auto_registry::Registry::open(&root) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("auto registry get: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match reg.get(&id, &out) {
                Ok(outcome) => {
                    let signature = match outcome.signature {
                        Some(true) => ", signature verified",
                        Some(false) => unreachable!("bad signatures are errors"),
                        None => ", unsigned",
                    };
                    println!(
                        "artifact {id} -> {} (content verified{signature})",
                        out.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("auto registry get: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn record(
    store_path: &Path,
    keep_jsonl: Option<&Path>,
    recover_partial: bool,
    command: &[String],
) -> ExitCode {
    let jsonl_path = match keep_jsonl {
        Some(p) => p.to_path_buf(),
        None => std::env::temp_dir().join(format!(
            "auto-trace-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        )),
    };

    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .env("AUTO_TRACE_FILE", &jsonl_path)
        .status();
    let status = match status {
        Ok(s) => s,
        Err(e) => {
            eprintln!("auto record: cannot run {:?}: {e}", command[0]);
            return ExitCode::FAILURE;
        }
    };

    let cleanup = || {
        if keep_jsonl.is_none() {
            let _ = std::fs::remove_file(&jsonl_path);
        }
    };

    if !jsonl_path.is_file() {
        eprintln!(
            "auto record: command produced no trace at {} — is it instrumented \
             with an auto SDK? (AUTO_TRACE_FILE was set for it)",
            jsonl_path.display()
        );
        return ExitCode::FAILURE;
    }

    let (trace, partial) = if recover_partial {
        match auto_trace::jsonl::parse_file_recovering(&jsonl_path) {
            Ok(rec) => {
                if let Some(d) = &rec.dropped {
                    eprintln!(
                        "auto record: torn tail recovered — dropped line {} ({} bytes): {}",
                        d.line, d.bytes, d.reason
                    );
                }
                let partial = rec.dropped.is_some();
                (rec.trace, partial)
            }
            Err(e) => {
                eprintln!("auto record: invalid trace {}: {e}", jsonl_path.display());
                cleanup();
                return ExitCode::FAILURE;
            }
        }
    } else {
        match auto_trace::jsonl::parse_file(&jsonl_path) {
            Ok(t) => (t, false),
            Err(e) => {
                eprintln!("auto record: invalid trace {}: {e}", jsonl_path.display());
                cleanup();
                return ExitCode::FAILURE;
            }
        }
    };
    let mut store = match auto_trace::Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "auto record: cannot open store {}: {e}",
                store_path.display()
            );
            cleanup();
            return ExitCode::FAILURE;
        }
    };
    let ingest_result = if partial {
        store.ingest_partial(&trace)
    } else {
        store.ingest(&trace)
    };
    if let Err(e) = ingest_result {
        eprintln!("auto record: ingest failed: {e}");
        cleanup();
        return ExitCode::FAILURE;
    }
    cleanup();

    let effectful = trace.spans.iter().filter(|s| s.kind.is_effectful()).count();
    let partial_note = if partial { " (PARTIAL: torn tail)" } else { "" };
    println!(
        "recorded trace {} task \"{}\": {} spans ({} effectful){partial_note} -> {}",
        trace.header.trace_id,
        trace.header.task,
        trace.spans.len(),
        effectful,
        store_path.display()
    );

    if status.success() {
        ExitCode::SUCCESS
    } else {
        eprintln!("auto record: command exited with {status}; trace was ingested anyway");
        ExitCode::FAILURE
    }
}

fn report(task: &str, store_path: &Path) -> ExitCode {
    if !store_path.is_file() {
        eprintln!("auto report: store {} does not exist", store_path.display());
        return ExitCode::FAILURE;
    }
    let store = match auto_trace::Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "auto report: cannot open store {}: {e}",
                store_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    match auto_trace::determinism::report(&store, task) {
        Ok(r) => {
            print!("{}", auto_trace::determinism::render(&r));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("auto report: {e}");
            ExitCode::FAILURE
        }
    }
}

fn verify(contract_path: &Path, store_path: &Path, runs_dir: &Path) -> ExitCode {
    let contract = match auto_contract::parse::load(contract_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "auto verify: cannot load contract {}: {e}",
                contract_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    if !store_path.is_file() {
        eprintln!("auto verify: store {} does not exist", store_path.display());
        return ExitCode::FAILURE;
    }
    let store = match auto_trace::Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "auto verify: cannot open store {}: {e}",
                store_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let report = match auto_contract::harness::verify_against_store(&contract, &store) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("auto verify: {e}");
            return ExitCode::FAILURE;
        }
    };
    print!("{}", auto_contract::harness::render(&report));

    match write_eval_run(&report, runs_dir) {
        Ok(()) => {}
        Err(code) => return code,
    }

    verdict_exit(report.verdict)
}

#[allow(clippy::too_many_arguments)] // mirrors the flag surface; grouped structs would hide it
fn compile(
    contract_path: &Path,
    store_path: &Path,
    module_path: Option<&Path>,
    synth: &str,
    frontier_model: &str,
    spend_cap_usd: &str,
    session: &str,
    guard_field: Option<&str>,
    guard_alpha_milli: u32,
    guard_embedding: bool,
    divergent_pick: &str,
    judge_model: Option<&str>,
    out_path: &Path,
    runs_dir: &Path,
) -> ExitCode {
    if divergent_pick == "weighted" {
        eprintln!(
            "auto compile: --divergent-pick weighted is distill-only (ADR-0031): \
             synthesis rejects conflicting observations by construction; use most-common"
        );
        return ExitCode::FAILURE;
    }
    if !matches!(divergent_pick, "refuse" | "most-common") {
        eprintln!(
            "auto compile: unknown --divergent-pick {divergent_pick:?} (refuse | most-common)"
        );
        return ExitCode::FAILURE;
    }
    let judge = match build_judge(judge_model, spend_cap_usd, session, "compile") {
        Ok(j) => j,
        Err(code) => return code,
    };
    if !matches!(synth, "enum" | "llm") {
        eprintln!("auto compile: unknown --synth {synth:?} (enum | llm)");
        return ExitCode::FAILURE;
    }
    if module_path.is_some() && synth == "llm" {
        eprintln!("auto compile: --module and --synth llm are mutually exclusive");
        return ExitCode::FAILURE;
    }
    let contract = match auto_contract::parse::load(contract_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "auto compile: cannot load contract {}: {e}",
                contract_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    if !store_path.is_file() {
        eprintln!(
            "auto compile: store {} does not exist",
            store_path.display()
        );
        return ExitCode::FAILURE;
    }
    let store = match auto_trace::Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "auto compile: cannot open store {}: {e}",
                store_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let (kind, name) = match contract.scope.clone() {
        auto_contract::Scope::Span { kind, name } => (kind, name),
        auto_contract::Scope::Region { from, to } => {
            return compile_region(
                &contract,
                &store,
                store_path,
                module_path,
                synth,
                guard_field,
                guard_alpha_milli,
                guard_embedding,
                out_path,
                runs_dir,
                &from,
                &to,
            );
        }
        auto_contract::Scope::Task => {
            eprintln!(
                "auto compile: task-scope contracts are not compilable yet \
                 (span or region scope required; see spec/artifact.md)"
            );
            return ExitCode::FAILURE;
        }
    };

    // the implementation: hand-supplied module, or a synthesized program +
    // embedded generic interpreter (S4 enumerative, or LLM-guided CEGIS)
    let (module_bytes, program_bytes, module_label, notes) = match module_path {
        Some(path) => {
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("auto compile: cannot read module {}: {e}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let notes = "Hand-assisted compile: the module was supplied, not synthesized. \
                 Latencies near timer resolution; no cost economics are claimed \
                 for this task."
                .to_owned();
            (bytes, None, label, notes)
        }
        None if synth == "llm" => {
            match synthesize_program_llm(
                &store,
                &contract,
                frontier_model,
                spend_cap_usd,
                session,
                divergent_pick,
            ) {
                Ok((program_json, notes)) => (
                    auto_passes::interpreter_wasm().to_vec(),
                    Some(program_json.into_bytes()),
                    "dsl-interpreter (llm-cegis)".to_owned(),
                    notes,
                ),
                Err(code) => return code,
            }
        }
        None => match synthesize_program(&store, &contract, divergent_pick) {
            Ok((program_json, s)) => {
                let notes = format!(
                    "S4 synthesized compile: enumerative search (not LLM-guided) found a \
                     {}-op DSL program (digest {}) over {} distinct input(s), exploring \
                     {} state(s) to depth {}. Program in program.json; generic interpreter \
                     embedded. Generalization is evidence-bounded by the distinct inputs. \
                     Latencies near timer resolution; no cost economics are claimed.",
                    s.program.ops.len(),
                    &auto_trace::model::digest_hex(&s.program.to_json())[..12],
                    s.distinct_inputs,
                    s.states_explored,
                    s.depth_reached,
                );
                (
                    auto_passes::interpreter_wasm().to_vec(),
                    Some(program_json.into_bytes()),
                    "dsl-interpreter (synthesized)".to_owned(),
                    notes,
                )
            }
            Err(code) => return code,
        },
    };

    verify_and_emit(
        EmitPlan {
            command: "compile",
            contract: &contract,
            kind: &kind,
            name: &name,
            store: &store,
            store_path,
            module_bytes,
            payload: program_bytes,
            module_label,
            notes,
            chain: None,
            capabilities: Vec::new(),
            tools: None,
            guard_field: guard_field.map(str::to_owned),
            guard_alpha_milli,
            guard_embedding,
            out_path,
            runs_dir,
        },
        judge,
    )
}

/// Region compile (spec/synthesis.md §8, ADR-0015): gather the recorded
/// chains, synthesize every stage and every non-identity glue edge, and run
/// the assembled pipeline through THE unchanged gate — differential replay
/// covers every recorded end-to-end chain.
#[allow(clippy::too_many_arguments)] // mirrors compile's flag surface
fn compile_region(
    contract: &auto_contract::Contract,
    store: &auto_trace::Store,
    store_path: &Path,
    module_path: Option<&Path>,
    synth: &str,
    guard_field: Option<&str>,
    guard_alpha_milli: u32,
    guard_embedding: bool,
    out_path: &Path,
    runs_dir: &Path,
    from: &str,
    to: &str,
) -> ExitCode {
    if module_path.is_some() {
        eprintln!(
            "auto compile: --module with a region contract is not supported in v0 \
             (a hand module cannot carry the chain's per-stage provenance)"
        );
        return ExitCode::FAILURE;
    }
    if synth == "llm" {
        eprintln!(
            "auto compile: --synth llm for regions is future work — per-edge \
             LLM-guided proposals need per-edge budget accounting (ADR-0015)"
        );
        return ExitCode::FAILURE;
    }

    let region = match auto_backend::differential::gather_region(store, contract) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("auto compile: {e}");
            return ExitCode::FAILURE;
        }
    };
    let disqualified: Vec<String> = region
        .gathered
        .disqualified()
        .map(|(i, g)| {
            format!(
                "chain input #{i}: {} distinct recorded output(s), {} recorded error(s)",
                g.outputs.len(),
                g.errors
            )
        })
        .collect();
    if !disqualified.is_empty() {
        eprintln!(
            "auto compile: recorded region behavior is not deterministic ({}); a \
             divergent chain cannot compile",
            disqualified.join("; ")
        );
        return ExitCode::FAILURE;
    }

    let budget = auto_passes::SearchBudget::default();
    let outcome = auto_passes::synthesize_region(
        &region.chain,
        &region.stage_pairs,
        &region.glue_pairs,
        budget,
    );
    let (pipeline, stages, glue_synthesized, glue_identity) = match outcome {
        auto_passes::RegionOutcome::Found {
            pipeline,
            stages,
            glue_synthesized,
            glue_identity,
        } => (pipeline, stages, glue_synthesized, glue_identity),
        auto_passes::RegionOutcome::EdgeRefused { edge, detail } => {
            eprintln!(
                "auto compile: region synthesis refused at {edge}: {detail} — every \
                 stage and every non-identity glue edge must synthesize"
            );
            return ExitCode::FAILURE;
        }
    };
    let chain_names: Vec<&str> = region.chain.iter().map(|(_, n)| n.as_str()).collect();
    let capabilities = pipeline.capabilities();
    println!(
        "region: {} stage(s) [{}] + {glue_synthesized} synthesized glue edge(s) \
         ({glue_identity} identity, omitted) -> {}-stage pipeline{}",
        stages,
        chain_names.join(" -> "),
        pipeline.stages.len(),
        if capabilities.is_empty() {
            String::new()
        } else {
            format!(" (capabilities: {})", capabilities.join(", "))
        },
    );

    // the gate's hermetic tool host: every recorded (tool, input) -> output
    // pair — replay invents nothing, and an unwitnessed pair fails the gate
    let tools = if capabilities.is_empty() {
        None
    } else {
        let mut replay = std::collections::BTreeMap::new();
        for (position, (kind, name)) in region.chain.iter().enumerate() {
            if kind == "tool_call" {
                for (input, output) in &region.stage_pairs[position] {
                    replay.insert(
                        (name.clone(), auto_trace::model::canonical_json(input)),
                        output.clone(),
                    );
                }
            }
        }
        Some(auto_runtime::executor::HostTools::Replay(replay))
    };

    let pipeline_json = pipeline.to_json();
    let notes = format!(
        "Region compile (ADR-0015): the recorded {from}..{to} chain [{}] synthesized \
         as a {}-program pipeline (digest {}) — {} stage(s), {glue_synthesized} glue \
         edge(s) synthesized, {glue_identity} witnessed identity and omitted. Pure \
         chain (model_call only); the enumerative per-edge budget applies to each \
         edge separately. Generalization is evidence-bounded by the distinct \
         recorded chains. Latencies near timer resolution; no cost economics are \
         claimed.",
        chain_names.join(" -> "),
        pipeline.stages.len(),
        &auto_trace::model::digest_hex(&pipeline_json)[..12],
        stages,
    );

    // pure pipelines embed the zero-import interpreter; capability
    // pipelines embed the tools build (the auto.tool_call import declared)
    let (module_bytes, module_label) = if capabilities.is_empty() {
        (
            auto_passes::interpreter_wasm().to_vec(),
            "dsl-interpreter (region pipeline)".to_owned(),
        )
    } else {
        (
            auto_passes::tool_interpreter_wasm().to_vec(),
            "dsl-tool-interpreter (capability region pipeline)".to_owned(),
        )
    };
    verify_and_emit(
        EmitPlan {
            command: "compile",
            contract,
            kind: "region",
            name: &format!("{from}..{to}"),
            store,
            store_path,
            module_bytes,
            payload: Some(pipeline_json.into_bytes()),
            module_label,
            notes,
            chain: Some(region.chain.clone()),
            capabilities,
            tools,
            guard_field: guard_field.map(str::to_owned),
            guard_alpha_milli,
            guard_embedding,
            out_path,
            runs_dir,
        },
        None,
    )
}

/// The shared gate-and-emit tail of `compile` and `distill`: THE gate. One
/// implementation so the two paths can never drift apart.
struct EmitPlan<'a> {
    command: &'static str,
    contract: &'a auto_contract::Contract,
    kind: &'a str,
    name: &'a str,
    store: &'a auto_trace::Store,
    store_path: &'a Path,
    module_bytes: Vec<u8>,
    /// init payload (program.json entry): DSL program or model json
    payload: Option<Vec<u8>>,
    module_label: String,
    notes: String,
    /// region scope only: the (kind, name) chain, lowered as one IR
    /// transform node per stage; None = span scope
    chain: Option<Vec<(String, String)>>,
    /// declared capabilities (sorted, unique) — the tool names a capability
    /// pipeline calls; empty = pure artifact (ADR-0017)
    capabilities: Vec<String>,
    /// the gate's hermetic tool host: recorded (name, input) -> output
    /// replay for capability pipelines; None for pure artifacts
    tools: Option<auto_runtime::executor::HostTools>,
    /// build a runtime guard from the witnessed inputs' text at this object
    /// field; None = unguarded artifact
    guard_field: Option<String>,
    /// split-conformal miscoverage alpha in thousandths for the guard
    /// threshold (ADR-0014); 1 = the v0-equivalent max quantile
    guard_alpha_milli: u32,
    /// wire v2 lexical trigram-hash cosine embeddings instead of the v0/v1
    /// trigram-Jaccard sketch (ADR-0023); opt-in, default = today's bytes
    guard_embedding: bool,
    out_path: &'a Path,
    runs_dir: &'a Path,
}

fn verify_and_emit(
    plan: EmitPlan,
    mut judge: Option<Box<dyn auto_contract::harness::Judge>>,
) -> ExitCode {
    let EmitPlan {
        command,
        contract,
        kind,
        name,
        store,
        store_path,
        module_bytes,
        payload,
        module_label,
        notes,
        chain,
        capabilities,
        tools,
        guard_field,
        guard_alpha_milli,
        guard_embedding,
        out_path,
        runs_dir,
    } = plan;

    let executor = match auto_runtime::WasmExecutor::from_parts_with_tools(
        &module_bytes,
        payload.clone(),
        tools,
    ) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("auto {command}: module rejected: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut subject = auto_runtime::WasmSubject::new(executor, module_label.clone());

    // contract verification (examples, eval cases, properties, budgets)
    let contract_report = auto_contract::harness::verify_against_subject_with_judge(
        contract,
        &mut subject,
        judge.as_deref_mut(),
    );
    // differential: every distinct recorded input must reproduce its output
    let differential = match auto_backend::differential::differential_check_with_judge(
        store,
        contract,
        &mut subject,
        judge.as_deref_mut(),
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("auto {command}: differential failed to run: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut checks = contract_report.checks.clone();
    checks.extend(differential.checks.clone());
    let verdict = auto_contract::harness::verdict_of(&checks);
    let combined = auto_contract::harness::VerificationReport {
        contract_id: contract_report.contract_id.clone(),
        task: contract.task.clone(),
        subject: format!(
            "{command} candidate wasm \"{module_label}\" vs trace-store {}",
            store_path.display()
        ),
        verdict,
        observations: contract_report.observations + differential.compiled_latencies_ms.len(),
        checks,
    };
    print!("{}", auto_contract::harness::render(&combined));

    let now_ms = unix_now_ms();
    let (run_path, run_id) =
        match auto_contract::evalrun::write_eval_run(&combined, now_ms, runs_dir) {
            Ok(ok) => ok,
            Err(e) => {
                eprintln!("auto {command}: cannot write eval run: {e}");
                return ExitCode::FAILURE;
            }
        };
    println!("eval run {run_id} -> {}", run_path.display());

    if combined.verdict != auto_contract::harness::Verdict::Pass {
        eprintln!(
            "auto {command}: emit blocked — verdict {} (a failing or inconclusive \
             contract never emits; CLAUDE.md)",
            combined.verdict
        );
        return ExitCode::FAILURE;
    }

    let mut compiled = differential.compiled_latencies_ms.clone();
    compiled.sort_unstable();
    let mut recorded = differential.recorded_latencies_ms.clone();
    recorded.sort_unstable();
    let manifest = auto_backend::Manifest {
        manifest_version: auto_backend::MANIFEST_VERSION,
        task: contract.task.clone(),
        scope_kind: kind.to_owned(),
        scope_name: name.to_owned(),
        interface_input: contract.interface.input.to_string(),
        interface_output: contract.interface.output.to_string(),
        capabilities,
        contract_id: combined.contract_id.clone(),
        eval_run_ids: vec![run_id.to_string()],
        provenance: auto_backend::Provenance {
            trace_ids: differential.trace_ids.clone(),
            reference: if kind == "region" {
                format!(
                    "recorded observations of task \"{}\" region {name}",
                    contract.task
                )
            } else {
                format!(
                    "recorded observations of task \"{}\" span {kind}({name})",
                    contract.task
                )
            },
            observations: differential.recorded_latencies_ms.len(),
        },
        measured: auto_backend::Measured {
            compiled_latency_ms_p50: percentile(&compiled, 50),
            compiled_latency_ms_p95: percentile(&compiled, 95),
            compiled_latency_ms_max: compiled.last().copied().unwrap_or(0),
            reference_recorded_latency_ms_p95: percentile(&recorded, 95),
        },
        notes,
    };

    let graph_air = match lowered_graph_air(
        &combined.contract_id,
        kind,
        name,
        chain.as_deref(),
        contract,
    ) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("auto {command}: IR lowering failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // runtime guard from the witnessed inputs (S6): calibrated on exactly
    // the evidence the gate just verified against
    let guard = match &guard_field {
        None => None,
        Some(field) => {
            let gathered = match auto_backend::differential::gather_observations(store, contract) {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("auto {command}: cannot gather witnesses for the guard: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let inputs: Vec<serde_json::Value> =
                gathered.groups.values().map(|g| g.input.clone()).collect();
            let built = if guard_embedding {
                auto_runtime::Guard::build_embedding(&inputs, Some(field), guard_alpha_milli)
            } else {
                auto_runtime::Guard::build_conformal(&inputs, Some(field), guard_alpha_milli)
            };
            match built {
                Ok(g) => {
                    if let Some(e) = &g.embedding {
                        println!(
                            "guard: {} witness doc(s), wire v2 trigram-hash cosine (dim 256; \
                             lexical, not semantic), threshold {} micros (split-conformal \
                             alpha {:.3})",
                            e.docs().len(),
                            e.threshold_distance_micros(),
                            f64::from(guard_alpha_milli) / 1000.0
                        );
                    } else {
                        let calibration = if guard_alpha_milli == 1 {
                            String::new() // the v0-equivalent max quantile
                        } else {
                            format!(
                                " (split-conformal alpha {:.3})",
                                f64::from(guard_alpha_milli) / 1000.0
                            )
                        };
                        println!(
                            "guard: {} witness(es), threshold {:.4}{calibration}",
                            g.witnesses.len(),
                            g.threshold
                        );
                    }
                    Some(g.to_json().into_bytes())
                }
                Err(e) => {
                    eprintln!("auto {command}: cannot build guard: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    let bytes = match auto_backend::emit::emit(
        auto_backend::emit::EmitInputs {
            manifest,
            module: module_bytes,
            graph_air: Some(graph_air),
            program: payload,
            guard,
        },
        &combined,
    ) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("auto {command}: emit refused: {e}");
            return ExitCode::FAILURE;
        }
    };
    let artifact_id = match auto_backend::Artifact::from_bytes(&bytes) {
        Ok(a) => a.id(),
        Err(e) => {
            eprintln!("auto {command}: emitted container failed self-check: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::write(out_path, &bytes) {
        eprintln!(
            "auto {command}: cannot write artifact {}: {e}",
            out_path.display()
        );
        return ExitCode::FAILURE;
    }
    println!("artifact {artifact_id} -> {}", out_path.display());
    ExitCode::SUCCESS
}

/// Gather the distinct deterministic observations of a span scope, refusing
/// honestly when none exist or the recorded behavior diverges/errors.
fn gather_ready_observations(
    store: &auto_trace::Store,
    contract: &auto_contract::Contract,
    command: &str,
    divergent_pick: &str,
) -> Result<Vec<auto_passes::Observation>, ExitCode> {
    let gathered = match auto_backend::differential::gather_observations(store, contract) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("auto {command}: cannot gather observations: {e}");
            return Err(ExitCode::FAILURE);
        }
    };
    if gathered.groups.is_empty() {
        eprintln!(
            "auto {command}: no recorded observations for this span — record the \
             task first (`auto record`)"
        );
        return Err(ExitCode::FAILURE);
    }
    if divergent_pick == "most-common" {
        // ADR-0018 amendment: an EXPLICIT operator choice — train on the
        // majority witness per input; errored groups are never trainable.
        // The declared acceptance threshold still decides the gate verdict.
        let (pairs, errored_skipped) = auto_backend::differential::pick_observations(&gathered);
        if pairs.is_empty() {
            eprintln!("auto {command}: nothing trainable after the canonical pick");
            return Err(ExitCode::FAILURE);
        }
        let divergent_resolved = gathered
            .groups
            .values()
            .filter(|g| g.errors == 0 && g.outputs.len() > 1)
            .count();
        println!(
            "canonical pick: {} trainable input(s), {divergent_resolved} divergent \
             reference(s) resolved to their majority witness, {errored_skipped} \
             errored group(s) skipped",
            pairs.len(),
        );
        return Ok(pairs
            .into_iter()
            .map(|(input, output)| auto_passes::Observation { input, output })
            .collect());
    }
    let disqualified: Vec<String> = gathered
        .disqualified()
        .map(|(i, g)| {
            format!(
                "input #{i}: {} distinct recorded output(s), {} recorded error(s)",
                g.outputs.len(),
                g.errors
            )
        })
        .collect();
    if !disqualified.is_empty() {
        eprintln!(
            "auto {command}: recorded behavior is not deterministic ({}); a \
             divergent signature refuses by default — declare [acceptance] and \
             pass --divergent-pick most-common to train on majority witnesses \
             (ADR-0018)",
            disqualified.join("; ")
        );
        return Err(ExitCode::FAILURE);
    }
    Ok(gathered
        .groups
        .values()
        .map(|g| auto_passes::Observation {
            input: g.input.clone(),
            output: serde_json::from_str(g.outputs.first().expect("agreeing group has an output"))
                .expect("canonical recorded output parses"),
        })
        .collect())
}

/// S5: fit a small specialist to the recorded observations via an external
/// trainer, then run it through the SAME gate as compile.
#[expect(
    clippy::too_many_arguments,
    reason = "one flat clap surface; grouping would obscure the flags"
)]
fn distill(
    contract_path: &Path,
    store_path: &Path,
    trainer: &str,
    model_kind: &str,
    input_field: Option<&str>,
    holdout: f64,
    seed: u64,
    min_holdout_accuracy: f64,
    guard_alpha_milli: u32,
    guard_embedding: bool,
    divergent_pick: &str,
    judge_model: Option<&str>,
    spend_cap_usd: &str,
    session: &str,
    out_path: &Path,
    runs_dir: &Path,
) -> ExitCode {
    if !matches!(divergent_pick, "refuse" | "most-common" | "weighted") {
        eprintln!(
            "auto distill: unknown --divergent-pick {divergent_pick:?} (refuse | most-common | weighted)"
        );
        return ExitCode::FAILURE;
    }
    let judge = match build_judge(judge_model, spend_cap_usd, session, "distill") {
        Ok(j) => j,
        Err(code) => return code,
    };
    // model-kind dispatch: validator for the trainer's output + the
    // interpreter embedded in the artifact
    type Validate = fn(&str) -> Result<(), String>;
    fn validate_tree(json: &str) -> Result<(), String> {
        auto_passes::auto_model::Model::from_json(json)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn validate_mlp(json: &str) -> Result<(), String> {
        auto_passes::auto_model::Mlp::from_json(json)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    let (validate, interpreter, kind_label): (Validate, &'static [u8], &'static str) =
        match model_kind {
            "tree" => (
                validate_tree,
                auto_passes::model_interpreter_wasm(),
                "decision-tree",
            ),
            "mlp" => (validate_mlp, auto_passes::mlp_interpreter_wasm(), "mlp"),
            other => {
                eprintln!("auto distill: unknown --model-kind {other:?} (tree | mlp)");
                return ExitCode::FAILURE;
            }
        };
    let contract = match auto_contract::parse::load(contract_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "auto distill: cannot load contract {}: {e}",
                contract_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let (kind, name) = match contract.scope.clone() {
        auto_contract::Scope::Span { kind, name } => (kind, name),
        auto_contract::Scope::Region { .. } => {
            eprintln!(
                "auto distill: region contracts distill via per-stage models is \
                 future work — compile synthesizes regions today (ADR-0015)"
            );
            return ExitCode::FAILURE;
        }
        auto_contract::Scope::Task => {
            eprintln!("auto distill: span scope required (see spec/distillation.md)");
            return ExitCode::FAILURE;
        }
    };
    if !store_path.is_file() {
        eprintln!(
            "auto distill: store {} does not exist",
            store_path.display()
        );
        return ExitCode::FAILURE;
    }
    let store = match auto_trace::Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "auto distill: cannot open store {}: {e}",
                store_path.display()
            );
            return ExitCode::FAILURE;
        }
    };

    // ADR-0031: weighted trains on EVERY witnessed output of non-errored
    // groups, weight = witness count; errored groups are never trainable.
    // Like most-common this selects training DATA only — the gate below is
    // unchanged.
    let rows: Vec<(serde_json::Value, serde_json::Value, usize)> = if divergent_pick == "weighted" {
        let gathered = match auto_backend::differential::gather_observations(&store, &contract) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("auto distill: cannot gather observations: {e}");
                return ExitCode::FAILURE;
            }
        };
        if gathered.groups.is_empty() {
            eprintln!(
                "auto distill: no recorded observations for this span — record the \
                 task first (`auto record`)"
            );
            return ExitCode::FAILURE;
        }
        let (rows, errored_skipped) = auto_backend::differential::weighted_observations(&gathered);
        if rows.is_empty() {
            eprintln!("auto distill: nothing trainable after the weighted pick");
            return ExitCode::FAILURE;
        }
        let divergent = gathered
            .groups
            .values()
            .filter(|g| g.errors == 0 && g.outputs.len() > 1)
            .count();
        println!(
            "weighted witnesses: {} training row(s) over {} input(s), {divergent} \
             divergent reference(s) contributing every witnessed output, \
             {errored_skipped} errored group(s) skipped",
            rows.len(),
            gathered.groups.len() - errored_skipped,
        );
        rows
    } else {
        let observations =
            match gather_ready_observations(&store, &contract, "distill", divergent_pick) {
                Ok(o) => o,
                Err(code) => return code,
            };
        observations
            .into_iter()
            .map(|o| (o.input, o.output, 1))
            .collect()
    };
    let trainer_cmd: Vec<String> = trainer.split_whitespace().map(str::to_owned).collect();
    if trainer_cmd.is_empty() {
        eprintln!("auto distill: --trainer is empty");
        return ExitCode::FAILURE;
    }
    let distilled = match auto_passes::distillation::distill_weighted_validated(
        &rows,
        &trainer_cmd,
        input_field,
        holdout,
        seed,
        min_holdout_accuracy,
        &validate,
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("auto distill: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "distilled: {} (train_accuracy {:.3} over {}, holdout_accuracy {:.3} over {}, classes {:?})",
        distilled.metrics.trainer,
        distilled.metrics.train_accuracy,
        distilled.metrics.train_n,
        distilled.metrics.holdout_accuracy,
        distilled.metrics.holdout_n,
        distilled.metrics.classes,
    );
    if let (Some(weighted_acc), Some(total)) = (
        distilled.metrics.weighted_train_accuracy,
        distilled.metrics.train_weight,
    ) {
        println!(
            "weighted witnesses: weighted_train_accuracy {weighted_acc:.3} over              total witness weight {total} (ADR-0031; holdout stays plain unweighted)"
        );
    }

    let notes = format!(
        "S5 distilled compile: {kind_label} specialist (model digest {}) trained by \
         {:?} on {} observation(s) with {} held out (measured train_accuracy {:.3}, \
         holdout_accuracy {:.3}). Holdout metrics are provenance; emission was gated \
         on the same contract + differential PASS as every artifact. Latencies near \
         timer resolution; no cost economics are claimed.",
        &auto_trace::model::digest_hex(&distilled.model_json)[..12],
        distilled.metrics.trainer,
        distilled.metrics.train_n,
        distilled.metrics.holdout_n,
        distilled.metrics.train_accuracy,
        distilled.metrics.holdout_accuracy,
    );
    let notes = if divergent_pick == "weighted" {
        format!(
            "{notes} Training data: weighted witnesses (ADR-0031) — one row per              witnessed output of every non-errored group, weight = witness count;              train counts are rows, not distinct inputs."
        )
    } else {
        notes
    };

    verify_and_emit(
        EmitPlan {
            command: "distill",
            contract: &contract,
            kind: &kind,
            name: &name,
            store: &store,
            store_path,
            module_bytes: interpreter.to_vec(),
            payload: Some(distilled.model_json.into_bytes()),
            module_label: format!("{kind_label}-interpreter (distilled)"),
            notes,
            chain: None,
            capabilities: Vec::new(),
            tools: None,
            // the model's text field doubles as the guard field: distilled
            // artifacts are always guarded on their witnessed inputs
            guard_field: input_field.map(str::to_owned),
            guard_alpha_milli,
            guard_embedding,
            out_path,
            runs_dir,
        },
        judge,
    )
}

/// S4: extract the implementation from the recorded observations. Refuses
/// honestly when the recorded behavior is not deterministic or the search
/// budget is exhausted.
/// The frontier judge (ADR-0019): semantic-equivalence verdicts from a
/// spend-capped model. Every call is capped and ledgered (purpose "judge");
/// a non-yes/no answer is a judge FAILURE, never a pass.
struct FrontierJudge<C: auto_frontier::Frontier> {
    inner: C,
    model: String,
}

impl<C: auto_frontier::Frontier> auto_contract::harness::Judge for FrontierJudge<C> {
    fn equivalent(
        &mut self,
        expected: &serde_json::Value,
        actual: &serde_json::Value,
        task_context: &str,
    ) -> Result<bool, String> {
        let request = auto_frontier::FrontierRequest {
            system: "You judge whether two outputs of the same task are semantically \
                     equivalent - same meaning and effect for the task, wording may \
                     differ. Reply with exactly one word: yes or no."
                .to_owned(),
            user: format!(
                "{task_context}\nexpected: {}\nactual: {}",
                auto_trace::model::canonical_json(expected),
                auto_trace::model::canonical_json(actual),
            ),
            max_output_tokens: 200,
        };
        let response = self.inner.complete(&request).map_err(|e| e.to_string())?;
        match response.text.trim().to_lowercase().as_str() {
            "yes" => Ok(true),
            "no" => Ok(false),
            other => Err(format!("judge answered neither yes nor no: {other:?}")),
        }
    }

    fn describe(&self) -> String {
        format!("frontier-judge:{} (capped, ledgered)", self.model)
    }
}

/// Construct the optional frontier judge from --judge-model (ADR-0019):
/// a capped OpenAI client with ledger purpose "judge".
fn build_judge(
    judge_model: Option<&str>,
    spend_cap_usd: &str,
    session: &str,
    command: &str,
) -> Result<Option<Box<dyn auto_contract::harness::Judge>>, ExitCode> {
    let Some(model) = judge_model else {
        return Ok(None);
    };
    match capped_openai(model, spend_cap_usd, session, "judge") {
        Ok(client) => Ok(Some(Box::new(FrontierJudge {
            inner: client,
            model: model.to_owned(),
        }))),
        Err(e) => {
            eprintln!("auto {command}: cannot construct the judge: {e}");
            Err(ExitCode::FAILURE)
        }
    }
}

/// Parse an exact decimal USD amount ("25", "0.25", "1.5") into µ$. Floats
/// never touch money: dollars and up-to-6 fractional digits, zero-padded.
fn parse_usd_to_micros(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (dollars, frac) = match text.split_once('.') {
        Some((d, f)) => (d, f),
        None => (text, ""),
    };
    if dollars.is_empty() && frac.is_empty() {
        return Err(format!("{text:?} is not a decimal USD amount"));
    }
    if frac.len() > 6 {
        return Err(format!(
            "{text:?} has more than 6 fractional digits; µ$ is the resolution"
        ));
    }
    let dollars: u64 = if dollars.is_empty() {
        0
    } else {
        dollars
            .parse()
            .map_err(|e| format!("{text:?}: bad dollars: {e}"))?
    };
    let mut padded = frac.to_owned();
    while padded.len() < 6 {
        padded.push('0');
    }
    let micros: u64 = if padded.is_empty() {
        0
    } else {
        padded
            .parse()
            .map_err(|e| format!("{text:?}: bad fractional part: {e}"))?
    };
    dollars
        .checked_mul(1_000_000)
        .and_then(|d| d.checked_add(micros))
        .ok_or_else(|| format!("{text:?} overflows µ$"))
}

/// Read `name` from ./.env (KEY=VALUE lines, `#` comments) without mutating
/// the process environment — the value is passed explicitly to the client.
/// Missing file or missing key is `None`; the real environment still wins
/// inside the provider's own fallback when this returns `None`.
fn dotenv_key(name: &str) -> Option<String> {
    let text = std::fs::read_to_string(".env").ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim() == name
        {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// The ONLY frontier construction in the CLI: an OpenAI provider wrapped in
/// the spend cap (ADR-0010). Key from ./.env or the environment; cap parsed
/// as exact decimal USD — 0, the flag default, refuses every paid call.
fn capped_openai(
    model: &str,
    spend_cap_usd: &str,
    session: &str,
    purpose: &str,
) -> Result<auto_frontier::CappedFrontier<auto_frontier::OpenAiFrontier>, String> {
    let cap = parse_usd_to_micros(spend_cap_usd).map_err(|e| format!("--spend-cap-usd: {e}"))?;
    let key = dotenv_key(auto_frontier::OPENAI_KEY_ENV);
    let provider = auto_frontier::OpenAiFrontier::new(model, key).map_err(|e| e.to_string())?;
    let ledger = auto_frontier::SpendLedger::from_env_or_default().map_err(|e| e.to_string())?;
    auto_frontier::CappedFrontier::new(provider, cap, session, purpose, ledger)
        .map_err(|e| e.to_string())
}

/// LLM-guided CEGIS (spec/synthesis.md §7): frontier-proposed candidates,
/// the unchanged evaluator as checker, the unchanged emit gate downstream.
#[allow(clippy::too_many_arguments)] // mirrors the flag surface
fn synthesize_program_llm(
    store: &auto_trace::Store,
    contract: &auto_contract::Contract,
    frontier_model: &str,
    spend_cap_usd: &str,
    session: &str,
    divergent_pick: &str,
) -> Result<(String, String), ExitCode> {
    let observations = gather_ready_observations(store, contract, "compile", divergent_pick)?;
    let mut frontier = match capped_openai(frontier_model, spend_cap_usd, session, "cegis") {
        Ok(client) => client,
        Err(e) => {
            eprintln!("auto compile: cannot construct the capped frontier client: {e}");
            return Err(ExitCode::FAILURE);
        }
    };
    let config = auto_passes::CegisConfig::default();
    match auto_passes::synthesize_llm(&observations, &mut frontier, &config) {
        auto_passes::CegisOutcome::Found {
            program,
            rounds_used,
            candidates_tried,
        } => {
            let spent = frontier
                .session_spent_usd_micros()
                .map(|s| format!("{s}"))
                .unwrap_or_else(|_| "unreadable".to_owned());
            println!(
                "llm-cegis: verified a {}-op program in {rounds_used} round(s) \
                 ({candidates_tried} candidate(s) tried); session spend {spent}µ$ \
                 (ledger: every call recorded)",
                program.ops.len(),
            );
            let notes = format!(
                "LLM-guided CEGIS compile (ADR-0010): {frontier_model} proposed; a \
                 {}-op DSL program (digest {}) verified against every distinct \
                 recorded input in {rounds_used} round(s), {candidates_tried} \
                 candidate(s) tried. Proposal generation is nondeterministic; \
                 acceptance ran the same evaluator, and this emit passed the same \
                 gate as every artifact. Latencies near timer resolution; no cost \
                 economics are claimed.",
                program.ops.len(),
                &auto_trace::model::digest_hex(&program.to_json())[..12],
            );
            Ok((program.to_json(), notes))
        }
        auto_passes::CegisOutcome::NoCandidateVerified {
            rounds_used,
            candidates_tried,
            last_error,
        } => {
            eprintln!(
                "auto compile: llm-cegis found no verifying program in {rounds_used} \
                 round(s) ({candidates_tried} candidate(s)); last failure: {last_error}"
            );
            Err(ExitCode::FAILURE)
        }
        auto_passes::CegisOutcome::FrontierRefused { error } => {
            eprintln!("auto compile: frontier refused: {error}");
            Err(ExitCode::FAILURE)
        }
        auto_passes::CegisOutcome::ConflictingObservations { detail } => {
            eprintln!(
                "auto compile: observations conflict — the signature is not \
                 deterministic; nothing to extract ({detail})"
            );
            Err(ExitCode::FAILURE)
        }
    }
}

/// Resident runner mode (spec/runtime.md §9): the module compiles once,
/// then one JSON value per stdin line answers one JSON object per stdout
/// line until EOF. Exit 0 at EOF; per-line abstention is an object with
/// "abstained":true, not a process exit code.
fn run_stdio(artifact_path: &Path, tools: &[String]) -> ExitCode {
    let bytes = match std::fs::read(artifact_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "auto run: cannot read artifact {}: {e}",
                artifact_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let live = match parse_tool_table(tools) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("auto run --stdio: {e}");
            return ExitCode::FAILURE;
        }
    };
    let runner = match auto_runtime::Runner::new_with_tools(&bytes, live) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("auto run --stdio: {e}");
            return ExitCode::FAILURE;
        }
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match runner.serve(stdin.lock(), stdout.lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("auto run --stdio: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parse repeated `--tool name=command` flags into a live tool table
/// (ADR-0017); an empty flag list is no host at all.
fn parse_tool_table(flags: &[String]) -> Result<Option<auto_runtime::executor::HostTools>, String> {
    if flags.is_empty() {
        return Ok(None);
    }
    let mut table = std::collections::BTreeMap::new();
    for flag in flags {
        let Some((name, command)) = flag.split_once('=') else {
            return Err(format!("--tool {flag:?} is not name=command"));
        };
        let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
        if name.is_empty() || argv.is_empty() {
            return Err(format!("--tool {flag:?} needs a name and a command"));
        }
        table.insert(name.to_owned(), argv);
    }
    Ok(Some(auto_runtime::executor::HostTools::Live(table)))
}

fn synthesize_program(
    store: &auto_trace::Store,
    contract: &auto_contract::Contract,
    divergent_pick: &str,
) -> Result<(String, auto_passes::Synthesis), ExitCode> {
    let observations = gather_ready_observations(store, contract, "compile", divergent_pick)?;
    match auto_passes::synthesize(&observations, auto_passes::SearchBudget::default()) {
        auto_passes::SearchOutcome::Found(synthesis) => {
            println!(
                "synthesized: {} op(s) over {} distinct input(s) ({} state(s) explored, depth {})",
                synthesis.program.ops.len(),
                synthesis.distinct_inputs,
                synthesis.states_explored,
                synthesis.depth_reached,
            );
            Ok((synthesis.program.to_json(), synthesis))
        }
        auto_passes::SearchOutcome::BudgetExhausted {
            states_explored,
            depth_reached,
        } => {
            eprintln!(
                "auto compile: synthesis budget exhausted ({states_explored} state(s), \
                 depth {depth_reached}) — no fitting program in the v0 DSL; supply \
                 --module (hand-assisted) or wait for a richer search"
            );
            Err(ExitCode::FAILURE)
        }
        auto_passes::SearchOutcome::ConflictingObservations => {
            eprintln!(
                "auto compile: observations conflict (same input, different outputs) — \
                 the signature is not deterministic; nothing to extract"
            );
            Err(ExitCode::FAILURE)
        }
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the flag surface
fn run_artifact(
    artifact_path: &Path,
    input_text: &str,
    tier0: Option<&str>,
    tools: &[String],
    spend_cap_usd: &str,
    session: &str,
    store_path: Option<&Path>,
) -> ExitCode {
    let bytes = match std::fs::read(artifact_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "auto run: cannot read artifact {}: {e}",
                artifact_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let artifact = match auto_backend::Artifact::from_bytes(&bytes) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("auto run: invalid artifact: {e}");
            return ExitCode::FAILURE;
        }
    };
    let manifest = match artifact.manifest() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("auto run: invalid manifest: {e}");
            return ExitCode::FAILURE;
        }
    };
    let input: serde_json::Value = match serde_json::from_str(input_text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("auto run: --input is not valid JSON: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(input_ty) = auto_contract::parse::parse_value_type(&manifest.interface_input) else {
        eprintln!(
            "auto run: manifest declares unknown input type {:?}",
            manifest.interface_input
        );
        return ExitCode::FAILURE;
    };
    if let Err(e) = auto_contract::conform::conforms(&input, &input_ty) {
        eprintln!(
            "auto run: input does not conform to declared type {}: {e}",
            manifest.interface_input
        );
        return ExitCode::FAILURE;
    }

    // S6: guarded artifacts decide tier-1 vs deopt BEFORE executing
    match artifact.entries.get(auto_backend::container::GUARD_ENTRY) {
        None => {
            eprintln!("auto run: no guard in artifact; running tier-1 unguarded");
        }
        Some(raw) => {
            let guard = match std::str::from_utf8(raw)
                .map_err(|_| "guard is not utf-8".to_owned())
                .and_then(|t| auto_runtime::Guard::from_json(t).map_err(|e| e.to_string()))
            {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("auto run: invalid guard: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match guard.evaluate(&input) {
                auto_runtime::GuardOutcome::Proceed {
                    distance,
                    threshold,
                } => {
                    eprintln!("guard: proceed (distance {distance:.4} <= {threshold:.4})");
                }
                auto_runtime::GuardOutcome::Trip {
                    reason,
                    distance,
                    threshold,
                } => {
                    let shown = distance
                        .map(|d| format!("{d:.4}"))
                        .unwrap_or_else(|| "n/a".to_owned());
                    eprintln!(
                        "guard tripped: {reason} (distance {shown} > threshold {threshold:.4})"
                    );
                    return deopt(&manifest, &input, tier0, spend_cap_usd, session, store_path);
                }
            }
        }
    }

    let live_tools = match parse_tool_table(tools) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("auto run: {e}");
            return ExitCode::FAILURE;
        }
    };
    let executor = match auto_runtime::WasmExecutor::from_artifact_with_tools(&artifact, live_tools)
    {
        Ok(x) => x,
        Err(e) => {
            eprintln!("auto run: cannot load module: {e}");
            return ExitCode::FAILURE;
        }
    };
    match executor.execute(&input) {
        Ok(output) => {
            println!("{}", auto_trace::model::canonical_json(&output));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("auto run: execution failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A tripped guard: answer via tier-0 (recording the observation — the
/// ratchet) or abstain honestly. Never answers with the compiled path.
fn deopt(
    manifest: &auto_backend::Manifest,
    input: &serde_json::Value,
    tier0: Option<&str>,
    spend_cap_usd: &str,
    session: &str,
    store_path: Option<&Path>,
) -> ExitCode {
    let Some(tier0) = tier0 else {
        eprintln!(
            "auto run: no tier-0 configured; refusing to answer with the compiled \
             path (calibrated abstention — supply --tier0 to deopt instead)"
        );
        return ExitCode::from(3);
    };
    let spec = match auto_runtime::tier0::Tier0Spec::parse(tier0) {
        Ok(spec) => spec,
        Err(e) => {
            eprintln!("auto run: {e}");
            return ExitCode::FAILURE;
        }
    };
    let canonical_input = auto_trace::model::canonical_json(input);
    let started = std::time::Instant::now();
    let answer = match &spec {
        auto_runtime::tier0::Tier0Spec::Command(argv) => {
            let (cmd, args) = argv.split_first().expect("parse rejects empty commands");
            let output = std::process::Command::new(cmd)
                .args(args)
                .arg(&canonical_input)
                .output();
            let output = match output {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("auto run: tier-0 failed to spawn ({tier0:?}): {e}");
                    return ExitCode::FAILURE;
                }
            };
            if !output.status.success() {
                eprintln!(
                    "auto run: tier-0 exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
                return ExitCode::FAILURE;
            }
            match serde_json::from_slice(output.stdout.trim_ascii()) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("auto run: tier-0 output is not JSON: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        auto_runtime::tier0::Tier0Spec::Frontier { model } => {
            // one spend-capped call (ADR-0010): cap 0 — the default — refuses
            let mut frontier = match capped_openai(model, spend_cap_usd, session, "tier0") {
                Ok(client) => client,
                Err(e) => {
                    eprintln!("auto run: cannot construct the capped frontier client: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match auto_runtime::tier0::frontier_answer(manifest, input, &mut frontier, 1024) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("auto run: frontier tier-0 failed: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Some(out_ty) = auto_contract::parse::parse_value_type(&manifest.interface_output)
        && let Err(e) = auto_contract::conform::conforms(&answer, &out_ty)
    {
        eprintln!(
            "auto run: tier-0 answer does not conform to declared type {}: {e}",
            manifest.interface_output
        );
        return ExitCode::FAILURE;
    }
    eprintln!("deopt: tier-0 answered in {duration_ms}ms");

    match store_path {
        None => {
            eprintln!(
                "deopt: no --store given; observation NOT ingested (the ratchet \
                 needs a store to grow the witness set)"
            );
        }
        Some(_) if manifest.scope_kind == "region" => {
            eprintln!(
                "deopt: tier-0 answered, but region observations cannot be ingested \
                 yet (a region witness is a CHAIN, and a deopt answer is one \
                 end-to-end pair — per-stage attribution is future work, ADR-0015)"
            );
        }
        Some(store_path) => {
            match ingest_deopt_observation(store_path, manifest, input, &answer, duration_ms) {
                Ok(trace_id) => {
                    eprintln!(
                        "deopt: observation ingested as trace {trace_id}; recompile to \
                     extend coverage (the ratchet: nothing figured out twice)"
                    );
                }
                Err(e) => {
                    eprintln!("auto run: could not ingest the deopt observation: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    println!("{}", auto_trace::model::canonical_json(&answer));
    ExitCode::SUCCESS
}

/// Record a tier-0 answer as a synthetic single-span trace so recompilation
/// sees it as a witness.
fn ingest_deopt_observation(
    store_path: &Path,
    manifest: &auto_backend::Manifest,
    input: &serde_json::Value,
    answer: &serde_json::Value,
    duration_ms: u64,
) -> Result<String, String> {
    use auto_trace::model::{Span, SpanId, SpanKind, Trace, TraceHeader, TraceId};

    let kind = SpanKind::from_wire(&manifest.scope_kind)
        .ok_or_else(|| format!("manifest scope kind {:?} unknown", manifest.scope_kind))?;
    let now = unix_now_ms();
    // unique trace id from time + pid (no rng dependency); collision odds
    // are digest-grade
    let seed = format!(
        "deopt-{}-{}-{now}",
        std::process::id(),
        manifest.contract_id
    );
    let hex = auto_trace::model::digest_hex(&seed);
    let trace_id = u128::from_str_radix(&hex[..32], 16).map_err(|e| e.to_string())?;

    let trace = Trace {
        header: TraceHeader {
            trace_id: TraceId(trace_id),
            task: manifest.task.clone(),
            started_at_ms: now,
            sdk: format!("auto-cli-deopt/{}", env!("CARGO_PKG_VERSION")),
            attrs: std::collections::BTreeMap::new(),
            // deopt traces carry no task-level I/O (ADR-0025)
            task_input: None,
            task_output: None,
        },
        spans: vec![Span {
            span_id: SpanId(1),
            parent_span_id: None,
            seq: 1,
            kind,
            name: manifest.scope_name.clone(),
            input: input.clone(),
            output: Some(answer.clone()),
            error: None,
            started_at_ms: now,
            duration_ms,
            attrs: std::collections::BTreeMap::new(),
        }],
    };
    let mut store = auto_trace::Store::open(store_path).map_err(|e| e.to_string())?;
    store.ingest(&trace).map_err(|e| e.to_string())?;
    Ok(trace.header.trace_id.to_string())
}

fn inspect(file: &Path) -> ExitCode {
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("auto inspect: cannot read {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    if bytes.starts_with(auto_backend::MAGIC) {
        return inspect_artifact(file, &bytes);
    }
    match auto_ir::from_bytes(&bytes) {
        Ok(graph) => {
            print!("{}", auto_ir::render(&graph));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("auto inspect: {}: invalid IR: {e}", file.display());
            ExitCode::FAILURE
        }
    }
}

fn inspect_artifact(file: &Path, bytes: &[u8]) -> ExitCode {
    let artifact = match auto_backend::Artifact::from_bytes(bytes) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("auto inspect: {}: invalid artifact: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    let manifest = match artifact.manifest() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("auto inspect: {}: invalid manifest: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    println!("artifact {}", artifact.id());
    print!("{manifest}");
    println!("entries:");
    for (name, data) in &artifact.entries {
        println!("  {name} ({} bytes)", data.len());
    }
    ExitCode::SUCCESS
}

fn write_eval_run(
    report: &auto_contract::harness::VerificationReport,
    runs_dir: &Path,
) -> Result<(), ExitCode> {
    match auto_contract::evalrun::write_eval_run(report, unix_now_ms(), runs_dir) {
        Ok((path, id)) => {
            println!("eval run {id} -> {}", path.display());
            Ok(())
        }
        Err(e) => {
            eprintln!("auto: cannot write eval run: {e}");
            Err(ExitCode::FAILURE)
        }
    }
}

fn verdict_exit(verdict: auto_contract::harness::Verdict) -> ExitCode {
    match verdict {
        auto_contract::harness::Verdict::Pass => ExitCode::SUCCESS,
        auto_contract::harness::Verdict::Fail => ExitCode::FAILURE,
        auto_contract::harness::Verdict::Inconclusive => ExitCode::from(2),
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Nearest-rank percentile over an ascending-sorted slice; 0 for empty input
/// (only reachable when there were zero timed calls, which a Pass gate makes
/// impossible for real compiles).
fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * pct).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// Lower the compiled span to a 3-node IR graph (Input → Transform → Output):
/// the compiled unit is deterministic and effect-free by construction (the
/// module has zero imports). GraphId derives from the contract id so the
/// lowering is reproducible.
/// Lower the compiled scope to an IR graph. A span scope is one transform
/// node between input and output; a region scope (`chain = Some(...)`) is
/// one transform node PER chain span, edges in seq order — the region's
/// structure made visible in the artifact's graph.air. Intermediate region
/// ports are typed `json` (glue values are witnessed, not declared).
fn lowered_graph_air(
    contract_id: &str,
    kind: &str,
    name: &str,
    chain: Option<&[(String, String)]>,
    contract: &auto_contract::Contract,
) -> Result<Vec<u8>, String> {
    let id_hex = contract_id.get(..32).ok_or("contract id too short")?;
    let graph_id = u128::from_str_radix(id_hex, 16).map_err(|e| e.to_string())?;
    let mut graph = auto_ir::Graph::new(
        auto_ir::GraphId(graph_id),
        format!("{}:{kind}:{name}", contract.task),
    );
    let input_ty = contract.interface.input.clone();
    let output_ty = contract.interface.output.clone();
    let json_ty =
        auto_contract::parse::parse_value_type("json").ok_or("the v0 type grammar lost `json`")?;

    // transform stages: the single span, or the region's chain
    let stages: Vec<(String, String)> = match chain {
        None => vec![(kind.to_owned(), name.to_owned())],
        Some(chain) => chain.to_vec(),
    };
    let last_stage = stages.len(); // node ids: 0 = input, 1..=n stages, n+1 = output

    graph
        .insert_node(
            auto_ir::Node::new(auto_ir::NodeId(0), "input", auto_ir::NodeKind::Input)
                .with_outputs(vec![auto_ir::Port::new("input", input_ty.clone())]),
        )
        .map_err(|e| e.to_string())?;
    for (position, (stage_kind, stage_name)) in stages.iter().enumerate() {
        let node_id = position as u64 + 1;
        let in_ty = if position == 0 {
            input_ty.clone()
        } else {
            json_ty.clone()
        };
        let out_ty = if position + 1 == stages.len() {
            output_ty.clone()
        } else {
            json_ty.clone()
        };
        graph
            .insert_node(
                auto_ir::Node::new(
                    auto_ir::NodeId(node_id),
                    stage_name,
                    auto_ir::NodeKind::Transform {
                        op: format!("compiled:{stage_kind}:{stage_name}"),
                    },
                )
                .with_inputs(vec![auto_ir::Port::new("input", in_ty)])
                .with_outputs(vec![auto_ir::Port::new("output", out_ty)]),
            )
            .map_err(|e| e.to_string())?;
    }
    graph
        .insert_node(
            auto_ir::Node::new(
                auto_ir::NodeId(last_stage as u64 + 1),
                "output",
                auto_ir::NodeKind::Output,
            )
            .with_inputs(vec![auto_ir::Port::new("output", output_ty)]),
        )
        .map_err(|e| e.to_string())?;
    for from in 0..=last_stage as u64 {
        graph
            .insert_edge(auto_ir::Edge {
                from: auto_ir::NodeId(from),
                from_port: 0,
                to: auto_ir::NodeId(from + 1),
                to_port: 0,
            })
            .map_err(|e| e.to_string())?;
    }
    auto_ir::to_bytes(&graph).map_err(|e| e.to_string())
}
