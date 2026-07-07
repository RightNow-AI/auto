# Auto artifacts — specification, v0

Status: v0, matches `crates/auto-backend` and `crates/auto-runtime` as
merged. Manifest format version: **0** (the `manifest_version` field).
Where prose and code disagree, the code wins; this document is written for
external readers.

An **artifact** is the output of a compile: the thing the runtime executes
instead of re-deriving behavior on a frontier model. Artifacts are only ever
produced downstream of a passing verification (§7) — an artifact that exists
is an artifact whose claims were measured.

## 1. what an artifact is

A `.cbin` is a container of named entries: today, **code** (one wasm module)
plus the **manifest** (the trust layer: measured guarantees, capability
ceiling, full provenance), conventionally plus the lowered IR graph of the
compiled unit. Small models and kernels join the container at S5+; signing
arrives at S7. One v0 artifact implements one span-scoped operation (a
recorded `kind` + `name` signature, spec/trace.md §2) — whole-task
compilation is later work.

## 2. container format

A deterministic named-entry blob. All integers little-endian:

```text
magic "ACB0" | u32 entry_count | entries...
entry: u32 name_len | name (utf-8) | u64 data_len | data
```

Entries are **sorted by name** (byte order) and names are **unique**, so
equal artifacts serialize to identical bytes. There is no compression, no
timestamp, no permission bits, no ordering freedom — nothing a repacker
could vary (rationale: spec/adr/0004-artifact-execution.md).

**Artifact id** = sha-256 (lowercase hex) of the container bytes. Content
addressing is the identity story: same entries, same bytes, same id.

| entry | status | contents |
|---|---|---|
| `manifest.json` | required | the manifest (§3), canonical JSON |
| `module.wasm` | required | the core wasm module (§4) |
| `graph.air` | conventional | the lowered IR graph (spec/ir.md §9) |

**Strict parse.** Readers reject: missing `ACB0` magic; truncation at any
field (entry count, name length, name, data length, data); non-utf-8 entry
names; names out of sorted order or duplicated; trailing bytes after the
last entry; a missing required entry. No best-effort reads.

## 3. the manifest

The `manifest.json` entry, read strictly (`manifest_version` must be exactly
**0**). Every number in it is either **measured during the compile that
emitted the artifact** or absent — never fabricated, never rounded up, never
a projection (CLAUDE.md: the manifest is never aspirational). The field-level
standard — semantics, honesty rules, consumer obligations, versioning — is
spec/manifest.md; the table below is its summary.

| field | meaning |
|---|---|
| `manifest_version` | exactly 0; any other value is rejected loudly |
| `task` | the task label traces and contracts carry |
| `scope_kind`, `scope_name` | the span signature this artifact implements |
| `interface_input`, `interface_output` | IR value type grammar strings (spec/ir.md §3) |
| `capabilities` | declared capability ceiling, sorted; **must be empty in v0** (§5) |
| `contract_id` | content id of the gating contract (spec/contract.md §8) |
| `eval_run_ids` | the **Pass** eval run(s) that allowed this emit (spec/contract.md §7) — the evidence behind every parity claim |
| `provenance.trace_ids` | traces whose recorded observations gated the emit |
| `provenance.reference` | plain description of the reference interpreter the artifact replaces |
| `provenance.observations` | recorded observations the differential check replayed |
| `measured.compiled_latency_ms_p50/p95/max` | wall-clock latencies of the compiled subject during the gating verification, on the emitting machine |
| `measured.reference_recorded_latency_ms_p95` | p95 of the *recorded* reference durations for the same signature |
| `notes` | plain-language caveats (e.g. toy-task economics); honesty in prose |

Honesty rules, restated as rules: measured numbers only; `eval_run_ids` cite
the Pass run that gated the emit — a parity claim without an eval run id is
invalid by constitution; `notes` carries the caveats a reader needs before
believing the numbers. The manifest body has its own sha-256 digest
(canonical JSON: sorted keys, compact separators), but the artifact's
identity is the digest of the whole container (§2).

## 4. ABI

The module ABI is **frozen** for v0; the S4 program extension (`init`,
below) is additive — an optional export, touching nothing existing. The
module is a core wasm module.

**Imports: zero.** The module has no imports of any kind. Any import means
the loader refuses the module (§5) — this is the v0 capability rule: only
pure artifacts exist, and their declared capabilities are empty.

**Exports:**

| export | type | role |
|---|---|---|
| `memory` | linear memory | the only data channel between host and module |
| `alloc` | `(i32 len) -> i32 ptr` | module hands the host a buffer to write into |
| `run` | `(i32 in_ptr, i32 in_len) -> i64` | execute once |
| `init` | `(i32 in_ptr, i32 in_len)` | optional: receive the program, once, before `run` (below) |

**Call sequence.** The host instantiates a fresh instance, calls
`alloc(in_len)`, writes the input bytes at the returned pointer, and calls
`run(in_ptr, in_len)`. The `run` result packs
`((out_ptr as u64) << 32) | (out_len as u64)`, bit-cast to `i64`; the host
reads `out_len` bytes at `out_ptr` from the exported memory.

**Program extension (`init`).** Synthesized artifacts (spec/synthesis.md)
carry a `program.json` entry, and their module — the embedded generic
interpreter — exports `init: (i32 in_ptr, i32 in_len)`. **Iff** the
artifact carries `program.json`, the host calls `init` **exactly once per
instance, before `run`**, with the same discipline as input bytes:
`alloc(prog_len)`, write the program bytes, bounds-check, call. The pairing
is checked both ways, and a mismatch is a load/execute error, never silent:
an artifact carrying a program whose module exports no `init` is refused,
and a module exporting `init` in an artifact carrying no program is
refused. An `init` trap is an execution failure like any other (below). S3
artifacts without programs are unchanged — no `init` export, no extra call.

**Byte payloads.** Input bytes are canonical JSON utf-8 of the input value;
output bytes are canonical JSON utf-8 of the output value (canonical JSON:
sorted object keys, compact separators — spec/trace.md §4). The host
bounds-checks both regions against the memory size, requires utf-8, and
requires JSON; violations are ABI errors, distinct from traps.

**Failure.** A trap — panic, `unreachable`, memory fault, fuel exhaustion —
is an **execution failure**. There is no in-band error convention: a module
that cannot answer traps, and the host reports it.

**Instance lifetime.** One `run` call per instance; the host instantiates
fresh per call, so **no cross-call state exists**. Modules may therefore
leak freely (no `dealloc` export exists): every allocation dies with the
instance.

## 5. capability confinement, v0

The IR carries capability effects (`net | fs | exec | secrets | payments`,
spec/ir.md §5); the artifact boundary is where they become physical. The v0
rule is total:

- the manifest's declared `capabilities` **must be empty**, and
- the module **must have zero imports**; the loader refuses any import at
  load time, before instantiation.

A wasm module can only reach the outside world through imports, so a pure
artifact **physically cannot** open a socket, touch a filesystem, spawn a
process, read a secret, or move money — there is nothing linked to reach
them with. This is confinement by construction, not by policy check.
WASI-backed capability grants for impure artifacts (declared IR effects
mapped to runtime handles) are future work
(spec/adr/open-questions.md, "artifacts & execution").

## 6. execution limits

Execution is bounded so verification cannot hang and a runaway module fails
loudly. Per `run` call, as merged in `auto-runtime`:

- **fuel:** 500,000,000 units (`FUEL_PER_CALL`). Fuel is wasmtime's
  deterministic instruction budget; exhaustion traps. A module that never
  terminates becomes an execution failure, never a hang.
- **memory:** 64 MiB cap (`MAX_MEMORY_BYTES`), enforced by the store
  limiter; growth past the cap is denied.

A trap from either limit is a failure of that execution, indistinguishable
in consequence from a panic (§4). The compiled module is reused across
calls, but every call runs in a fresh store and instance with its own fuel
and memory budget — nothing persists between calls. The constants are
engine-level verification bounds, not per-artifact declarations; per-node
resource bounds in the IR (spec/ir.md §7) remain declarations.

## 7. the emit gate

Artifacts exist only downstream of a **Pass** verdict (spec/contract.md §6).
The gate is mechanical, and it refuses on any of:

- **verdict Fail** — something checked was violated;
- **verdict Inconclusive** — nothing was violated but a normative claim went
  unchecked. Inconclusive is not Pass. **Fail and Inconclusive both block
  emit**; unchecked is never rounded up to emitted;
- the manifest citing a different contract than the one the gating report
  verified;
- a non-empty declared capability set (v0, §5).

There is no force flag.

**Differential rule.** Before the verdict is folded, every **distinct
recorded input** of the contract's span scope is replayed through the
candidate module, and the module's output must equal the recorded output
under canonical-JSON equality. Two hard cases:

- an input whose recorded outputs already disagree across observations (or
  whose observations errored) fails outright and the candidate is never run
  on it — **divergent recorded signatures refuse to compile**, because a
  divergent reference is not evidence to compare against;
- zero matching recorded spans is an unchecked claim (→ Inconclusive at
  best), never a silent pass.

The compiled and recorded latencies collected during this gating run are
what the manifest's `measured` block reports; the Pass eval run's id is what
`eval_run_ids` cites. The gate and the evidence are the same event.

## 8. what S3 does not claim

- **The module is hand-supplied.** S3 permits hand-assisted passes: a human
  wrote `evals/toy-agent/fake-frontier-impl`. Automated symbolic extraction
  (CEGIS, sandboxed verification) is S4; nothing here pretends to synthesize.
- **Core wasm module, not the component model.** The constitution names the
  wasm component model as the backend target; v0 deliberately ships a core
  module with the §4 ABI and records the migration
  (spec/adr/0004-artifact-execution.md).
- **No cost economics.** The manifest measures latencies of the toy task on
  one machine. No dollar-cost parity claim is made — the toy's reference
  "model" is a local function, so cost numbers would be theater; `notes`
  says so in any toy artifact.
- **No signing, no registry.** Content addressing (§2) is the identity
  story today; sigstore signing and the registry arrive S7.
- **No models, no kernels.** The container carries one wasm module; onnx/
  gguf entries arrive with distillation (S5).
