# Auto runtime — tiered execution, v0

Status: v0, matches `crates/auto-runtime` and the `auto run` surface of
`crates/auto-cli` as merged. Guard wire-format version (the
`guard_version` field): **1** written by default, **2** written only on
explicit opt-in (embedding guards, ADR-0023); **0, 1, and 2** read (§2,
ADR-0014, ADR-0023). Where prose and code disagree, the code wins; this
document is written for external readers.

The **runtime** is where a compiled artifact meets inputs it was never
verified on. The emit gate (spec/artifact.md §7) proves an artifact
reproduces its *witnessed* behavior — nothing more. The tiered runtime is
the answer to everything else: a **guard** on the compiled entry decides,
per input, whether the compiled path's evidence covers this input; within
coverage the artifact runs (**tier-1**), beyond it execution falls back to
the interpreter (**tier-0**), the interpreter's answer is recorded as a new
observation, and recompilation folds it back in — **the ratchet: nothing
figured out twice**. A wrong "stay compiled" decision is a silent
correctness failure, which is why the guard is a first-class component and
why its v0 limits are stated in this document rather than papered over.

## 1. the tiering model

- **tier-1** — the artifact's wasm module under the frozen ABI, fuel and
  memory limits, and zero-imports confinement (spec/artifact.md §4–§6).
- **tier-0** — the interpreter. The constitution binds tier-0 to a frontier
  model; **v0 does not have that binding**: it requires API access under the
  frontier-spend cap, and no cap plumbing or authorized spend exists
  (spec/adr/open-questions.md, "tiering (S6)"). v0's tier-0 is a **pluggable
  command** (§3) — `evals/toy-agent/tier0_oracle.py` is the reference — and
  nothing in the runtime pretends otherwise.

Decision order in `auto run`: parse the artifact → check the input conforms
to the manifest's declared input type (a nonconforming input is a run
error, not a guard trip) → evaluate the guard, if the artifact carries one
→ **proceed** runs tier-1; **trip** deopts to tier-0 (§3) or abstains (§5).
An artifact without a `guard.json` entry runs tier-1 **unguarded, loudly**
(a stderr notice on every run): v0 guards are opt-in at compile time
(`compile --guard-field`; `distill` reuses its `--input-field` as the guard
field when one is given).

## 2. the guard

A guard is calibrated at compile time from exactly the evidence the emit
gate verified against — the **witnesses**, the distinct recorded inputs —
and carried in the artifact as the `guard.json` entry
(spec/artifact.md §2). Wire form: canonical JSON, strict parse. Three wire
versions exist: writers emit **v1** (`guard_version` 1, split-conformal
calibration, ADR-0014) by default, or **v2** (embedding guards, ADR-0023,
§ wire v2 below) only on explicit opt-in; readers also accept **v0**
(leave-one-out max, ADR-0007) with unchanged semantics.

| field | meaning |
|---|---|
| `guard_version` | **0** or **1**; any other value is rejected loudly |
| `kind` | must be `"trigram_jaccard_nn"` — the only guard kind |
| `input_field` | optional: object field holding the guarded text; absent = the input itself must be the text (omitted when absent, never `null`) |
| `witnesses` | one sketch per witness: the **sorted, deduplicated** fnv1a-32 hashes of the text's char trigrams (`auto_model::trigram_hashes` — lowercase, 3-char windows, spaces included; a text under 3 chars sketches empty) |
| `threshold` | trip boundary in `[0, 1]` (§ calibration below) |
| `calibration` | **v1 only — required there, forbidden on v0.** Metadata about how `threshold` was derived; evaluation never reads it |

v1 `calibration` object, exactly three fields:

| field | meaning |
|---|---|
| `method` | exactly `"split_conformal"` |
| `alpha_milli` | nominal miscoverage rate in integer thousandths, in `(0, 1000)` exclusive; 100 (alpha 0.1) is the default choice |
| `scores_n` | number of nonconformity scores calibrated on: the witness count for n ≥ 2, `0` for a lone witness |

**Strict parse.** Readers reject: `guard_version` ∉ {0, 1, 2}; an unknown
`kind`; unknown fields, top-level or inside `calibration`; zero witnesses;
an unsorted or duplicated sketch; a threshold outside `[0, 1]`; a
`calibration` field on v0 (absent is the only valid v0 form, `null`
included) or a missing or `null` one on v1; a `method` other than
`split_conformal`; an `alpha_milli` outside `(0, 1000)`; a `scores_n`
that does not match the witness count. A guard that cannot be trusted
must not gate tier-1. No best-effort reads.

**Distance.** The input's text is sketched with the same trigram rule; the
guard's distance is the **minimum Jaccard set distance**
`1 − |A ∩ B| / |A ∪ B|` from that sketch to any witness — nearest-neighbor
over sketches. Edge cases are defined, not accidental: two empty sketches
are distance 0.0 (identical, if vacuous); exactly one empty is 1.0
(nothing shared).

**Calibration — split conformal over leave-one-out scores (v1).** The
nonconformity score of witness `w_i` is its Jaccard distance to its
nearest *other* witness — the same leave-one-out distances v0 computed.
The threshold is the standard split-conformal quantile of those n scores
at nominal miscoverage `alpha = alpha_milli / 1000`: the **k-th smallest
score, `k = ceil((n+1)(1−alpha))`** — the `(n+1)` is the finite-sample
correction (Angelopoulos & Bates, arXiv:2107.07511). k is computed
exactly on integer thousandths; the result is clamped to `[0, 1]`. Two
defined departures where the textbook quantile is undefined or unsafe:

- **k > n** — the correction exceeds the sample. Textbook conformal calls
  this quantile +∞, i.e. admit everything; the guard truncates to the
  **maximum score** instead — exactly v0's leave-one-out max, so small
  witness sets behave as v0 did. (Threshold = max score iff k ≥ n, which
  holds iff `alpha_milli · (n+1) < 2000`: up to n = 18 at the default
  alpha 0.1.) The truncation's failure direction is more trips, never a
  wider admission.
- **one witness** — no leave-one-out scores exist; threshold **0.0**,
  maximally conservative: only trigram-identical inputs proceed.

`Guard::build` (what `compile --guard-field` uses) calibrates at
`alpha_milli` 1, the most conservative expressible level, whose quantile
is the maximum score for every n ≤ 1998 — thresholds there are
bit-identical to v0's leave-one-out max (at larger n the quantile can
only sit lower: more trips, never fewer). `Guard::build_conformal` takes
an explicit `alpha_milli`. A v0 document parses to the same evaluation
behavior it always had and re-serializes byte-identically — no alpha is
invented for it.

**What the calibration guarantees — precisely.** Split conformal's
guarantee is conditional on **exchangeability**: *if* a future input were
exchangeable with the witnesses (informally: drawn from the same
distribution that produced them, order carrying no information), it would
pass the guard with probability ≥ 1−alpha. That conditional is the entire
claim. Arbitrary future inputs are **not** exchangeable with the
witnesses — an out-of-distribution input is exactly the case where the
assumption fails — so **no coverage bound exists for OOD traffic and none
is claimed**. That asymmetry is why tripping is the safe direction: an
in-distribution input wrongly tripped costs one deopt; there is no
statistical statement under which an OOD input is owed admission. alpha
therefore tunes the expected **deopt rate on in-distribution traffic**,
not correctness. One honest nuance: the calibration scores are
leave-one-out — each witness scored against the other n−1 — while a live
input is scored against all n witnesses, which can only shrink the live
distance; the slack is in the passing direction, so the ≥ 1−alpha reading
survives.

**Evaluation is total and never proceeds unguarded.** `distance ≤
threshold` (inclusive) proceeds; anything greater trips. An input with no
text to measure — not a string, not an object, missing field, non-string
field — **trips** with no distance, it never passes by default and never
errors. Evaluation is identical for v0 and v1 guards: calibration only
changed how `threshold` was derived.

**Wire v2 — embedding guards (ADR-0023, opt-in).** The constitution names
"embedding-distance OOD"; v2 is its first honest rung: a **dense lexical
embedding**, not a semantic one. `Guard::build_embedding` (the CLI's
`--guard-embedding` opt-in, composing with `--guard-field` and
`--guard-alpha-milli`) emits `guard_version` 2:

| field | meaning |
|---|---|
| `guard_version` | **2** |
| `field` | optional: object field holding the guarded text (v0/v1's `input_field` meaning; omitted when absent, never `null`) |
| `witnesses` | the **raw witness docs** — strings, sorted (byte order) and deduplicated. Vectors are recomputed from these at build and at load: **no floats cross the wire** |
| `embedding.method` | exactly `"trigram_hash_cosine"` |
| `embedding.dim` | exactly **256**, pinned for this method |
| `embedding.threshold_distance_micros` | trip boundary as **u32 cosine-distance micros**, in `[0, 2_000_000]` |
| `embedding.calibration` | the same three-field split-conformal object as v1 (`method`, `alpha_milli`, `scores_n`) |

*Featurizer (frozen).* Slide a 3-byte window over the doc's raw utf-8
bytes (**byte** trigrams — deliberately not v1's lowercased char
trigrams; a different featurizer is a different wire version, never a
silent reinterpretation); hash each window with FNV-1a **64-bit** (offset
14695981039346656037, prime 1099511628211); bucket = `hash % 256`; sign =
bit 32 of the hash, zero-indexed (set = +1, clear = −1); accumulate
signed counts (i64, exact); L2-normalize to f32. Deterministic,
dependency-free, order-independent — a bag of trigrams. A doc under 3
bytes (or a full signed cancellation) embeds to the zero vector, never
fake-normalized.

*Distance and decision — fixed point.* The input embeds the same way;
distance is the minimum over witnesses of **cosine distance** `1 − dot`
(unit vectors, so range `[0, 2]`), converted exactly once, at one pinned
boundary, to u32 micros: round half up, clamp to `[0, 2_000_000]`. The
decision compares integers — `micros ≤ threshold_distance_micros`
proceeds, anything greater trips. No f64 in the wire, no
platform-dependent float comparison in the decision; outcomes report
`micros / 1e6` so callers see exactly the compared quantities.
Zero-vector edges mirror v1's empty-sketch rules: both zero → 0.0,
exactly one zero → 1.0.

*Calibration is the same rule as v1*, over the same leave-one-out
construction: witness i's score is the micros distance to its nearest
other witness; the threshold is the k-th smallest score at
`k = ceil((n+1)(1−alpha))` — one shared implementation of k, not a fork —
with the same defined departures (k > n truncates to the max score; one
distinct doc calibrates to 0 micros). Everything above about the
guarantee carries over **verbatim**: coverage is conditional on
exchangeability, no claim exists for OOD traffic, alpha tunes the
in-distribution deopt rate.

*Lexical, not semantic — the load-bearing caveat.* **v2 upgrades the
geometry, not the meaning: the vector is built from the same byte
trigrams the text is spelled with, so a paraphrase with disjoint
vocabulary still trips.** Cosine over dense signed counts weights shared
trigram mass (and, unlike Jaccard's sets, repetition) — nothing more.
Semantic embeddings (an in-process onnx encoder, e.g. MiniLM) stay a
recorded upgrade: they need a model-distribution story, and gates run
with no network, so nothing that must download weights can be the tested
path (ADR-0023).

*Version rules (byte-stability is a hard invariant).* v0 and v1 documents
read exactly as they did before v2 existed and re-serialize
byte-identically — never rewritten, never upgraded. v2 is emitted only on
explicit opt-in; `Guard::build` / `Guard::build_conformal` still emit v1.
Readers refuse loudly: an unknown `embedding.method` or a `dim` other
than 256 is an error, **never a silent fallback to Jaccard**; zero
witness docs, a non-sorted/deduplicated doc list, a threshold above
2,000,000, a float or negative threshold, and unknown fields all refuse
to load. Fail-closed, as everywhere else in this section.

**Honest limits, plainly.**

- The split-conformal calibration is real, but the guarantee is exactly
  the conditional stated above — it says nothing about OOD inputs, and a
  handful of witnesses is still a thin calibration set (at n ≤ 18 and the
  default alpha the quantile is just the leave-one-out max). The
  embedding-distance half of the constitution's named design now has its
  first rung (wire v2, ADR-0023) — but that rung is **lexical geometry
  only**; the *semantic* embedder remains a recorded upgrade with its own
  dependency decision (inference stack, model distribution — deferral
  recorded in ADR-0014 and again in ADR-0023).
- **A witness sharing no trigram with any other calibrates the threshold
  to 1.0 at any alpha** — every leave-one-out score is 1.0, so every
  quantile of them is 1.0, and such a guard admits everything. Disjoint
  witnesses are read as evidence that "anything goes"; a useful threshold
  needs witnesses with real neighbors (the toy e2e records near-variant
  documents for exactly this reason).
- The distance is **lexical, not semantic** — on every wire version, v2
  included: a paraphrase with new vocabulary trips (safe — costs one
  deopt), while an in-vocabulary scramble proceeds. The guard asks "have
  we seen these trigrams", not "does this mean the same thing".
- A trigramless input (< 3 chars) matches any trigramless witness at
  distance 0.

## 3. deopt — the two tier-0 forms

A trip with `--tier0` configured deopts. The spec string has two forms
(`auto_runtime::tier0::Tier0Spec`):

**A command** (any spec not starting with the reserved `frontier:` prefix):
one command string, **split on whitespace** (no shell quoting); the runtime
appends **exactly one argument**: the canonical JSON of the input
(spec/trace.md §4). The command's contract:

- print the output value as **JSON on stdout**; exit **0**;
- anything else — nonzero exit, non-JSON stdout — is a run failure, loudly.

**A frontier model** (`frontier:<model-id>`): the constitution's named
tier-0. The runtime frames the model as the reference implementation of the
manifest's task/scope/interface and hands it the same canonical input JSON
the command form receives; the answer must be a single JSON value (one
markdown fence pair is tolerated; for a declared `text` output a bare-prose
answer is accepted as the string value). Exactly one call, no retries.
Spend is governed by the capped client (ADR-0010): the cap defaults to 0 —
**fail-closed, every paid call refused** — and is raised only by an explicit
owner-authorized `--spend-cap-usd`; every call lands in the append-only
spend ledger first. A cap or key refusal is a tier-0 failure, never an
invented answer.

Either way the answer is **unverified reference authority**, parsed and
**conformance-checked against the manifest's declared output type** before
it is accepted; a nonconforming answer is a failure, never a silently
relayed wrong shape. On a tier-0 failure the command's stderr (or the
frontier error) is relayed; wall-clock duration is measured and recorded
(§4). The toy oracle (`evals/toy-agent/tier0_oracle.py`) implements the
command contract over the recorded reference rule.

## 4. observation ingestion and the ratchet

With `--store`, a tier-0 answer becomes a **synthetic single-span trace**
ingested into the trace store: task, scope kind, and scope name from the
manifest; the run input as the span input; the tier-0 answer as its output;
the measured tier-0 duration; SDK label **`auto-cli-deopt/<version>`**, so
deopt-derived observations are never mistaken for SDK-recorded agent runs.
Without `--store` the answer is still returned but **not ingested**, and
the runtime says so — the ratchet needs a store to grow the witness set.

**The ratchet.** `auto compile` over the grown store folds the new
observation into *both* halves of the artifact: it is synthesis evidence
(the program must now reproduce it; the same emit gate applies) **and** a
guard witness (the once-novel input is now at distance 0.0). The recompiled
artifact answers that input on tier-1 with no tier-0 configured — nothing
figured out twice. In v0 the **`auto-daemon` crate** closes this loop as a
service (spec/adr/0013): it polls the store's distinct-input count for one
contract scope and, when the count grows past an **in-memory watermark**, runs
an **operator-configured recompile command** — the real `compile`/`distill`
pipeline as a subprocess, so the emit gate stays exactly the gate — then
publishes the emitted artifact to the content-addressed registry. A `once` mode
runs a single cycle for scripts and the e2e. Stated limits: the recompile is
still whole-artifact from scratch (incremental resynthesis is an open
question), the watermark is process memory only (a restart recompiles once
redundantly, which content-addressing dedupes), and the daemon is a library not
yet wired as an `auto` subcommand (§7).

What is captured is the **I/O observation**, not a full tier-0 execution
trace. The constitution's deopt captures the interpreter's trace; a
one-span synthetic trace is the v0 floor of that, honestly labeled.

## 5. abstention

A trip with **no** tier-0 configured **abstains**: exit code 3, no output,
and a plain refusal on stderr. Answering with the compiled path beyond its
calibration would be exactly the silent correctness failure guards exist to
prevent, so there is no override flag. "Calibrated abstention" carries the
§2 caveats verbatim: a split-conformal quantile over leave-one-out scores
whose guarantee is conditional on exchangeability (§2, ADR-0014) — the
runtime abstains *as calibrated*, and the distance is still lexical.

## 6. exit codes (`auto run`)

| code | meaning |
|---|---|
| 0 | answered — tier-1 in-distribution, or tier-0 deopt |
| 3 | calibrated abstention — guard tripped, no tier-0 configured |
| 1 | everything else: unreadable artifact/guard, nonconforming input, tier-1 execution failure, tier-0 protocol failure, ingestion failure |
| 2 | argument-parse errors (clap's convention) |

## 7. what v0 does not claim

- **Frontier tier-0 answers are unverified.** The binding exists (§3,
  ADR-0010) but a frontier answer is reference authority checked only for
  interface conformance at ingestion — no cross-model agreement, no retry,
  no self-consistency. Trust grows only through the ratchet: the answer
  becomes a witness, and recompilation gates on the full contract.
- **No guards on latency or cost.** The guard measures input distance only;
  a slow tier-1 call or an expensive tier-0 trips nothing. Latency/cost
  guards need measured per-call budgets enforced at run time.
- **The recompile daemon is opt-in and, by default, blunt.** `auto daemon`
  (ADR-0013) closes the deopt→recompile loop by polling a store and running the
  operator-configured compile command; nothing recompiles unless an operator
  started it. Its watermark is in-memory by default and **optionally
  persistent** (`watermark_path`: the last-compiled count survives a restart, so
  a same-count cycle no-ops even across processes; a corrupt watermark file is a
  loud error, never a silent skip). A failed cycle stops it loudly by default;
  **supervised mode** (`supervise`) instead logs and retries a *retryable* fault
  (store/recompile/artifact/publish) after an exponential backoff, while a
  config-shaped fault (unloadable contract, misconfigured recompile argv, corrupt
  watermark) still stops it. Both defaults reproduce the wave-4 in-memory,
  fail-loud behavior (ADR-0013 amendment, wave 5).
- **No semantic OOD and no coverage guarantee** (§2, stated twice on
  purpose).
- **One guarded text field.** Multi-field and structured-input guards are
  future work.

Rationale and alternatives: spec/adr/0007-tiering-guards.md.

## 8. auto serve — the runtime as a process

`auto serve` (crate `auto-serve`) turns a local registry into a long-running,
**read-only, tier-1-only** HTTP server. It serves artifacts the registry
already holds; it never compiles, signs, records, or recompiles. Blocking
`tiny_http`, one request at a time, one log line per request.

Endpoints (all responses `application/json`):

| method + path | 200 body | other |
|---|---|---|
| `GET /health` | `{"ok":true,"artifacts":<count>}` | 500 if the registry cannot be listed |
| `GET /artifacts` | `{"artifacts":[{"id","task","scope"[,"problem"]}]}` from the live listing | — |
| `POST /run/<id>` | `{"output":<value>}` (guard proceeds → tier-1) | 409 abstain, 400 bad JSON, 404 unknown id, 500 registry/exec/uncovered-capability fault |

Every artifact is loaded through `Registry::get`, so its content id (and any
signature) is re-verified on first load; the parsed guard and compiled module
are then cached by id (sound because ids pin bytes).

**Abstain, not deopt.** A guarded artifact evaluates its guard first (§2).
Proceed runs tier-1; **trip returns `409 {"abstained":true,"reason",
"distance","threshold"}`** — the same calibrated abstention as `auto run`
without `--tier0` (§5). There is **no in-server tier-0**: a per-request
frontier deopt needs a spend policy (who authorizes, which cap, charged to
whom) that does not exist (§1, ADR-0010 caps are per-CLI-session).

**Capability artifacts and the tool table (ADR-0017).** An artifact whose
manifest declares capabilities loads through a **server-wide tool table** the
operator supplies at startup (`auto serve --tool name=command …`, the same
grammar as `auto run --tool`). The flags are parsed once; a malformed flag is a
startup error, raised before the socket binds. Each capability artifact loads
through that one table and the loader enforces coverage — every declared
capability must have a tool, or that artifact fails to load and its `/run`
answers `500` with the loader's message (the missing tool named). **Pure
artifacts are unaffected:** they load with no host even when the server carries
a table (the loader refuses a host on a pure artifact), byte-for-byte the
pre-table behavior. The operator picks one table for the whole server; a
**per-request** tool/effect policy — who may invoke which tool, charged to whom
— stays unresolved and is the deliberate residual gap (§1, ADR-0017).

**What v0 does not do:** no tier-0 / deopt / recompile; no input conformance
check (unlike `auto run` — a guard trip or a tier-1 failure covers it); no TLS;
no auth; no concurrency (sequential loop); no hot reload or cache eviction
(a removed artifact answers from cache until restart); no clean-shutdown path.
All recorded upgrades. Rationale and alternatives: spec/adr/0011-serve-daemon.md.

## 9. the resident runner

`auto_runtime::Runner` (src/runner.rs) is the pipe-flavored sibling of §8's
server: one artifact, held resident, answering a line protocol on stdio. It
exists to kill the wave-5 systems finding — compiled `run` is sub-millisecond,
but a one-shot `auto run` pays process spawn plus module compilation every
call, and those dominate. The module is compiled **once**, at `Runner::new`,
and reused across every line; each answer still runs in a fresh wasm instance
(frozen ABI: one `run` per instance), so only the compile is amortized, no
cross-call state leaks.

Protocol: **one JSON value in per line, one JSON object out per line.** For
each input line `answer` mirrors §6's `auto run` decision and §8's object
shapes: unparseable JSON → `{"error":…}`; a guarded artifact whose guard trips
→ `{"abstained":true,"reason","distance","threshold"}` (§2, §5); otherwise
tier-1 executes → `{"output":<value>}` or `{"error":…}`. `serve` loops lines
until EOF and flushes after every response, so a caller blocked on the pipe
always reads a whole line. Like §8 there is **no tier-0**: a trip abstains,
never deopts, for the same per-request spend-policy gap (§1). Like §8 the input
is not conformance-checked against the manifest type — the guard trip or a
tier-1 failure covers it. **Capability artifacts** (a nonempty manifest
`capabilities`, ADR-0017) load through a tool table handed to
`Runner::new_with_tools`; the loader enforces coverage — every declared
capability needs a tool — and refuses a host on a pure artifact. `Runner::new`
supplies no table, so a pure artifact loads unchanged and a capability artifact
refuses through the loader, naming its missing tools. As with §8 the table is
chosen by whoever constructs the runner, not per line (the same residual
per-request gap). Intended use: an agent spawns the runner as a resident child
process. The `auto run --stdio` flag that wires the process's stdin/stdout — and
its `--tool` flags into `new_with_tools` — to it is owned by the CLI.
