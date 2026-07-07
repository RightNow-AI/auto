# ADR-0017: capability imports — the confinement promise made physical

status: accepted · scope: `auto-dsl` (Stage/pipeline v1), `dsl-interpreter` (tools build), `auto-runtime` (HostTools, loaders), `auto-backend` (emit rules, region gathering), `auto-cli` (--tool, capability compile), `evals/tool-agent`

## context

Regions refused tool calls (ADR-0015 decision 4) because compiling them
honestly requires the constitution's central mechanism: artifacts whose
wasm imports are EXACTLY their declared capabilities, loader-enforced. The
manifest's capability list was "empty by construction"; this ADR makes it
an enforced allowlist.

## decision

1. **One import.** The capability ABI is a single host function,
   `auto.tool_call(ptr, len) -> u64`: the module passes
   `{"name","input"}`, the host answers an envelope `{"ok"}|{"err"}`
   written into guest memory via the guest's own `alloc`. Tool failures
   are err envelopes the interpreter turns into honest traps; host
   infrastructure failures trap directly.
2. **Two interpreter builds from one source.** The pure build declares no
   imports (pure artifacts stay physically pure, byte-identical to wave
   4); the `tools` feature build declares the import. build.rs compiles
   both; the compiler embeds by need.
3. **Pipeline wire v1, lowest-version emission.** Stages are
   `{program}|{tool_call:{name}}`; pure pipelines still emit v0 bytes.
   A pipeline's tool names ARE the manifest capabilities (sorted, unique
   — emit refuses non-canonical lists).
4. **Loader rules.** No host: zero imports (unchanged). Host provided:
   imports must be exactly `auto.tool_call`; artifact loading cross-checks
   manifest capabilities against the host's table (every declared name
   covered) and refuses a host on a pure artifact. A capability artifact
   without a host refuses, naming the missing tools.
5. **The gate stays hermetic.** Verification replays tools from the
   RECORDED (name, input) -> output pairs; an unwitnessed pair errors —
   replay invents nothing, and no live tool runs inside the gate. Live
   runs provide tools as `--tool name=command` (the tier-0 command
   contract). `auto serve` still refuses capability artifacts (per-request
   tool/effect policy unresolved); the resident runner refuses them
   pending a tool-table constructor.
6. **Region purity relaxed exactly this far:** chains may contain
   model_call and tool_call spans; env_read/memory_op/branch still refuse.

## alternatives considered

**Per-tool imports** (one wasm import per capability): richer signatures,
but the import SET would vary per artifact, complicating the loader and the
one-implementation claim; a single dispatch import with name-level
allowlisting in the host keeps both simple and enforced. **WASI**: the real
component-model target, recorded since ADR-0004; this ABI is the minimal
honest step that does not fake WASI. **Host re-execution without imports**
(rejected in ADR-0015): confinement-free orchestration in disguise.

## consequences

- "A binary physically cannot exceed its declared capabilities" is now
  true for tool-calling agents, not just pure functions: undeclared
  imports refuse at load, undeclared tool names refuse in the host.
- The gate's tool replay makes capability verification hermetic and
  CI-safe (proven in `evals/tool-agent/e2e.sh`).
- serve/runner capability support and richer tool schemas (streaming,
  binary payloads) are recorded, not built.

## amendment — wave 7: serve + resident runner made servable

status: accepted · scope adds: `auto-serve`
(`ServeConfig.tools`, `ServeError::Config`, server-wide table),
`auto-runtime` (`Runner::new_with_tools`)

Decision 5 recorded that `auto serve` and the resident runner refused
capability artifacts "pending a tool-table constructor". That constructor now
exists; both are servable, with the loader still the single enforcement point.

- **One loader, delegated to.** `Runner::new_with_tools` and `auto-serve`'s
  load path both call `WasmExecutor::from_artifact_with_tools`, so every
  cross-check (coverage, `auto.tool_call`-only imports, no-host-on-a-pure-artifact)
  has one implementation. `Runner::new` and a pure server pass `None`: a pure
  artifact loads, a capability artifact refuses through the loader, naming its
  missing tools. This replaces the wave-6 hand-rolled runner refusal.
- **One server-wide table, operator-chosen.** `auto serve --tool name=command`
  (the `auto run --tool` grammar, parsed once at startup — a malformed flag is a
  startup `ServeError::Config`, before the socket binds) builds the single Live
  table every capability artifact loads through. The loader enforces per-artifact
  coverage; an uncovered capability artifact is a per-artifact load failure
  surfaced as `500` on `/run` with the loader's message. Pure artifacts pass
  `None` regardless of the table and are byte-for-byte unaffected.

**Residual gap, deliberate.** The table is chosen by whoever runs the server (or
constructs the runner); **per-REQUEST** tool/effect policy does not exist. A
requester cannot select or scope tools — who may invoke which tool, charged to
whom, is unresolved (the same shape as the per-request tier-0 spend gap,
spec/runtime.md §8). Richer tool schemas (streaming, binary payloads) remain
recorded, not built.
