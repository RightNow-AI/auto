# Auto manifests — the standard, v1

Status: **v1 of the manifest standard** (the S7 spine item: the spec opens),
matching `crates/auto-backend` as merged. The wire format this standard
governs is **`manifest_version` 0** — the only version any shipped reader
accepts; document version and wire version move independently, so prose can
sharpen without a format break. Where prose and code disagree, the code
wins; this document is written for external readers. The container that
carries the manifest is specified in spec/artifact.md.

The **manifest** is the artifact's trust layer: measured guarantees,
capability ceiling, full provenance. Its one governing rule, from the
constitution: **it is never aspirational, only measured.** Every number in a
manifest was measured during the compile that emitted the artifact, or the
field for it does not exist in the format at all. A manifest is not
marketing attached to a binary — it is the evidence record that lets a
stranger decide whether to trust one.

## 1. encoding and identity

One JSON document, the required `manifest.json` entry of every `.cbin`
(spec/artifact.md §2). Canonical form: sorted object keys, compact
separators. Read **strictly**: `manifest_version` must be exactly **0**;
any other value is rejected loudly. No best-effort reads.

The manifest body has its own sha-256 digest (of the canonical JSON), but
the **artifact's identity is the digest of the whole container**, manifest
included — there is no way to change a manifest without changing the
artifact id. Registries address artifacts by that container id
(spec/registry.md).

## 2. fields

| field | meaning |
|---|---|
| `manifest_version` | exactly 0; exact-match read (§5) |
| `task` | the task label traces and contracts carry (spec/trace.md §1) |
| `scope_kind`, `scope_name` | the span signature this artifact implements (spec/trace.md §2) — one v0 artifact implements one span-scoped operation |
| `interface_input`, `interface_output` | IR value type grammar strings (spec/ir.md §3); the runtime conformance-checks inputs and tier-0 answers against them |
| `capabilities` | declared capability ceiling, sorted; **must be empty in v0** — only pure artifacts exist, and the loader separately refuses any module import (spec/artifact.md §5) |
| `contract_id` | content id of the gating contract (spec/contract.md §8) |
| `eval_run_ids` | the **Pass** eval run(s) that allowed this emit (spec/contract.md §7) — the evidence behind every claim |
| `provenance.trace_ids` | traces whose recorded observations gated the emit |
| `provenance.reference` | plain description of the reference interpreter the artifact replaces |
| `provenance.observations` | recorded observations the differential check replayed |
| `measured.compiled_latency_ms_p50` / `_p95` / `_max` | wall-clock latencies of the compiled subject during the gating verification, on the emitting machine |
| `measured.reference_recorded_latency_ms_p95` | p95 of the *recorded* reference durations for the same signature |
| `notes` | plain-language caveats; honesty in prose (§3) |

## 3. honesty rules

Normative, restated as rules:

- **Measured or absent.** A number in a manifest was measured during the
  gating run that emitted the artifact — never fabricated, never rounded
  up, never a projection. What was not measured has no field: v0 carries
  no cost-per-call numbers because nothing measures cost yet, and adding
  the field before the measurement exists would be aspiration.
- **Eval-run citation.** `eval_run_ids` cite the Pass run(s) that gated the
  emit. A parity, latency, or correctness claim without an eval run id is
  **invalid by constitution** — the gate and the evidence are the same
  event (spec/artifact.md §7), and the run record is what makes "verified"
  checkable rather than asserted.
- **Notes discipline.** `notes` carries what a reader needs *before*
  believing the numbers: toy-task economics ("the reference model is a
  local function; cost parity here would be theater"), synthesis provenance
  (distinct inputs, states explored), distillation holdout metrics. Notes
  are prose provenance for facts the format has no fields for yet — they
  never soften a failing number, because a failing number blocks emit
  before a manifest exists.

## 4. what a consumer MUST verify

A consumer — a registry, a runtime, a human — trusting an artifact must
check, in order:

1. **Content id.** Recompute sha-256 over the container bytes and compare
   to the id the artifact was requested by. A mismatch is tampering or
   corruption; refuse (registries do this on every `get`,
   spec/registry.md §2).
2. **Strict manifest parse.** `manifest_version` exactly 0, canonical
   shape. An unreadable or wrong-version manifest is a refusal, not a
   warning.
3. **Contract + eval-run linkage.** `contract_id` names the gating
   contract; `eval_run_ids` must be non-empty. Where the consumer can
   resolve eval-run records (spec/contract.md §7), each cited run must
   exist, cite the same `contract_id`, and carry verdict **Pass** — a
   manifest citing a run it cannot substantiate is claiming, not proving.
4. **Capability ceiling.** v0: `capabilities` empty **and** the module has
   zero imports; either violation refuses before instantiation
   (spec/artifact.md §5).

Signatures (spec/registry.md §3) bind an identity to the container bytes;
they add *who stored this*, never a substitute for any check above.

## 5. versioning policy

`manifest_version` is read **exact-match**: this build accepts exactly
**0**. Any field addition, removal, or semantic change bumps the version
with an ADR — the same policy as `contract_version`, `dsl_version`,
`model_version`, and the IR schema version. There is no minor-version
leniency and no unknown-field tolerance: a reader never guesses what a
manifest meant.

Known pressures already on the books for the next bump
(spec/adr/open-questions.md): queryable synthesis/distillation provenance
fields (today: `notes` prose), measured cost fields (blocked on cost
capture existing at all), capability grants for impure artifacts, and the
wasm component-model migration (a format break by design, ADR-0004).
