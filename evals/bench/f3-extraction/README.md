# f3-extraction — AUTO-BENCH F3: typed-field extraction (the parser claim, head-on)

One real gpt-5.4-mini call per run (`agent.py`): customer-support email in,
a typed JSON object `{"order_id","category","urgency"}` out. The span
records the **parsed object** (not the reply string), the call's own
measured usage on the reserved attrs, and its real API latency. Task label
`bench-f3`; span `model_call/extract`; input `{"email": ...}`.

**Operator-run, never CI-run.** Every record makes one paid call
(~60 µ$ at the pinned prices). Session `bench-f3`, under the
benchmark-wide $5.00 cap (DESIGN.md pre-registers F3's recording at
≈ $0.005). All commands below are Git Bash from the repo root;
`STORE`/`SESSION`/`CAP` are parameters the operator sets.

## frozen corpus (`corpus.jsonl`, 40 items — committed output, not a generator)

Design rules (frozen with the file):

- one JSON object per line: `{"id": "f3-NNN", "email": "..."}` — no label
  fields on purpose: the recorded model is the reference, and committed
  "intent" labels would invite accuracy confusion the census never claims;
- every email contains **exactly one** order id matching `ORD-[0-9]{5}`,
  all 40 distinct;
- categories interleaved round-robin (refund, replacement, status, cancel,
  other; 8 each) so any prefix subsample stays balanced (F6 reuses F3);
- urgency by an explicit, human-agreeable cue vocabulary — high **iff** the
  email says urgent / immediately / ASAP / right away; low **iff** it says
  no rush / no hurry / whenever-style deferral; normal = no time-pressure
  wording at all. Distribution: 15 high (items 1–15), 15 normal (16–30),
  10 low (31–40). Non-high emails never contain a high cue and vice versa —
  ambiguity here would poison the determinism census;
- exactly one unambiguous ask per email (no "refund or replace, whichever");
  lengths 1–6 sentences; ASCII, single-line, no double-quote characters
  (argv-safe through Git Bash on the Windows host).

## protocol (record → fill examples → census → verify → compile → probes)

```sh
AUTO=./target/debug/auto      # or target/release/auto
STORE=bench-f3.db             # fresh store for the benchmark run
SESSION=bench-f3              # ledger session for any paid compile/judge leg
CAP=0.25                      # per-command spend cap, orchestrator-owned

# 1) record: 2 passes x 40 = 80 paid calls (the >=2-witness requirement)
for pass in 1 2; do
  while IFS= read -r line; do
    email=$(printf '%s' "$line" | python -c "import json,sys; sys.stdout.write(json.load(sys.stdin)['email'])")
    "$AUTO" record --store "$STORE" -- python evals/bench/f3-extraction/agent.py "$email"
  done < evals/bench/f3-extraction/corpus.jsonl
done

# 2) fill the contract examples from recorded reality (REQUIRED before any
#    verify/compile): replace the two "<FROM-RECORDED-REALITY ...>"
#    placeholders in extraction.contract.toml with the exact recorded span
#    outputs for items f3-001 / f3-002 (TOML inline tables). Commit the fill.

# 3) census — H2, the determinism report (measured, never extrapolated)
"$AUTO" report --task bench-f3 --store "$STORE"

# 4) verify the contract against the recordings (trace mode)
#    exit 0 = PASS, 1 = FAIL, 2 = INCONCLUSIVE
"$AUTO" verify --contract evals/bench/f3-extraction/extraction.contract.toml \
  --store "$STORE" --runs-dir evals/runs
```

### compile attempt order (each refusal is a result, not noise)

```sh
CONTRACT=evals/bench/f3-extraction/extraction.contract.toml

# (a) enum synthesis — EXPECTED HONEST REFUSAL: the output is a
#     per-input-varying json object and the v0 DSL has no object
#     constructor; expect "synthesis budget exhausted ... no fitting
#     program in the v0 DSL".
"$AUTO" compile --contract "$CONTRACT" --store "$STORE" --synth enum \
  --guard-field email --out f3-extraction.cbin --runs-dir evals/runs

# (b) LLM-CEGIS — OPTIONAL, PAID (orchestrator fires it; ADR-0010 rails).
#     Same DSL downstream, so the expected outcome is NoCandidateVerified;
#     the ledgered refusal is the result.
"$AUTO" compile --contract "$CONTRACT" --store "$STORE" --synth llm \
  --frontier-model gpt-5.4-mini --spend-cap-usd "$CAP" --session "$SESSION" \
  --guard-field email --out f3-extraction.cbin --runs-dir evals/runs

# (c) distill, tree — MEASURED EXPECTATION (probed locally with
#     synthetic object-output observations, no API): the v0 trainer refuses
#     non-string labels — tree_train.py exit 2, "output must be a JSON
#     string (the label), got object" -> auto distill reports TrainerFailed.
"$AUTO" distill --contract "$CONTRACT" --store "$STORE" \
  --trainer "python crates/auto-passes/trainer/tree_train.py" \
  --model-kind tree --input-field email --holdout 0 \
  --out f3-extraction.cbin --runs-dir evals/runs
```

If (a), (b), and (c) all refuse, F3's H3 row **is** "honest refusal at
every rung" — the sharpest form of the family's finding: the behavior is
witnessed-deterministic (H2) yet the v0 output algebra (text-shaped DSL,
string-label trainers) stops exactly at structured extraction. Report the
refusals verbatim; do not reshape the recording to fit a rung.

Divergence note: with 2 witnesses per input, any input the model answered
two ways makes observation-gathering refuse by default; rerun (a)/(c) with
`--divergent-pick most-common` (legal — the contract declares acceptance
950‰, which still decides the gate).

### H4 probes (`probes.jsonl` — chosen over inlining so the loop is machine-readable)

10 committed probes: 5 `heldout` (in-distribution shape, order ids unseen
in the corpus) + 5 `ood` (kubernetes log, original doggerel poetry,
"hi", a go panic, and a near-OOD restaurant email with no order id — the
interesting guard case). Design-intent fields for the heldout probes
(design documentation, not measured ground truth): p01
refund/high/ORD-62918, p02 replacement/normal/ORD-48350, p03
status/low/ORD-87621, p04 cancel/normal/ORD-15789, p05
other/normal/ORD-93046.

Runs only if some rung emitted `f3-extraction.cbin`:

```sh
# guard-only pass: exit 0 = tier-1 answered, exit 3 = abstained
while IFS= read -r line; do
  pid=$(printf '%s' "$line" | python -c "import json,sys; print(json.load(sys.stdin)['id'])")
  inp=$(printf '%s' "$line" | python -c "import json,sys; print(json.dumps({'email': json.load(sys.stdin)['email']}))")
  "$AUTO" run --artifact f3-extraction.cbin --input "$inp"
  echo "$pid exit=$?"
done < evals/bench/f3-extraction/probes.jsonl
```

H4 accounting per DESIGN.md: `ood` answered compiled = false-proceed (the
cardinal failure); `heldout` abstained = false-abstain. For the
deopt-answer-correctness column add `--tier0 "frontier:gpt-5.4-mini"
--spend-cap-usd "$CAP" --session "$SESSION" --store "$STORE"` (paid,
orchestrator only; deopts ingest back into the store — the ratchet). For
the v2 guard wire, re-run (c) with `--guard-embedding` (and the
benchmark-wide alpha pair via `--guard-alpha-milli`) to produce the second
artifact — keep alphas identical across families.

## why the contract declares no properties

The v0 property set is closed (`len_range`, `regex`, `num_range`,
`json_has_keys`, `one_of`) and has no predicate over json object **field
values** — no field regex, no field enum — so the pinned output shape is
not declarable as properties. (`json_has_keys` exists and could declare
key *presence* only; this family's spec declares none, and any addition is
legal only before the first compile.) Shape is enforced upstream instead:
the agent validates every reply, an invalid reply is a recorded span
error, and a single errored observation already fails verification via the
"observations free of recorded errors" check.

## errored recordings

`auto record` ingests the trace even when the agent exits nonzero — an
errored span is data. Two kinds, distinguishable in the span's error
string: `RuntimeError: api NNN ...` is transport/infra noise and may be
re-recorded into a **fresh** store before any gate run (logged in the
experiment log); `ValueError: model reply ...` is the model's own behavior
— the family's honest invalid-output rate — and is never scrubbed. Usage
attrs are recorded even on errored spans: the money was spent.
