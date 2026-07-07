# ADR-0018: statistical acceptance — declared differential agreement threshold, integer-milli, id-bearing

status: accepted · scope: `crates/auto-contract` (format
+ pure math), `crates/auto-backend` (applies it in the differential gate and
records the measured rate in the manifest), `spec/contract.md` §2/§8.

## context

Wave 5 measured the toy suite's `summarize` signature at 20% deterministic:
the reference interpreter, re-asked the same input, reproduces one output
for only a fraction of recorded inputs. The differential gate today is
binary-exact — an input whose recorded outputs already disagree fails the
agreement claim outright ("recorded outputs agree for input #i": Failed,
subject never run) — so a genuinely divergent signature can never emit, no
matter how well a compiled subject tracks the reproducible majority. The
open-questions ledger recorded this twice ("Statistical acceptance bounds",
"Judged / statistical match modes"): accepting divergence needs a
contract-level declaration, never a distillation-local gate bypass.

## decision

1. **One declared threshold, differential-only.** New optional
   `[acceptance]` table, one key: `differential_min_agreement_milli`, an
   integer in `1..=1000` — thousandths, ADR-0014's integer-milli
   convention; no floats in normative wire. It relaxes exactly one claim:
   the differential reproduction rate. Examples, properties, budgets, and
   interface conformance stay exact — an author who wants a fuzzy example
   writes no example.
2. **Absent = exact, 1000 = declared-exact, 0 = unwritable.** No table (or
   no key) keeps v0 semantics unchanged: every replayed input must
   reproduce its recorded output, divergent references hard-fail per input.
   `1000` is legal — the declared form of exactness (only total agreement
   satisfies it). `0` would declare "no agreement required", a vacuous
   gate: rejected loudly, as are values above 1000, non-integers, and
   unknown keys (strict TOML throughout, matching the rest of the format).
3. **Pure integer acceptance math in the harness.**
   `agreement_check(matched, eligible, min_milli) -> Check`: **Passed** iff
   `matched * 1000 >= min_milli * eligible` (widened to u128 — no overflow,
   no floats, no rounding); **Failed** otherwise; **Unchecked** when
   `eligible == 0` — no differential observations; partial data never
   passes. The check detail reports the measured rate as
   `matched/eligible` with a truncated percent — measured numbers, never
   rounded up. Verdict folding stays in `verdict_of`; the gate decides what
   counts as matched and eligible.
4. **Id-bearing.** Acceptance is threaded into the canonical contract-id
   JSON the way budgets is (a canonical-form table whose absent keys are
   omitted): two contracts differing only in acceptance make different
   normative claims and get different ids — a relaxed reproduction claim
   must never masquerade under an exact contract's id. One deliberate
   departure from budgets' always-present empty table: an **undeclared
   acceptance omits the whole table** from the canonical form. Budgets was
   born with the format and never had ids to preserve; acceptance lands on
   a live corpus of cited ids (eval runs, manifests, artifacts), and an id
   must change only when the normative claims do — every pre-acceptance
   contract keeps its id, pinned by test.
5. **`contract_version` stays 0.** The table is additive and the parser is
   strict: every released build rejects `[acceptance]` as an unknown key,
   loudly. An old reader can refuse an acceptance-bearing contract but can
   never silently misread one — strictness substitutes for the version
   bump the open-questions entry anticipated. The bump rule targets silent
   best-effort reads; there are none.

## alternatives considered

**Per-example match modes** (`match = "statistical"`). An example is one
normative case; a statistical claim over a single case is incoherent.
Agreement is a property of the replayed input population, so the
declaration belongs at contract level. Judged per-example comparison for
generative outputs remains a separate open item.

**LLM-judged comparison.** A judge model scoring semantic equivalence of
divergent outputs would accept more than string agreement honestly can.
It needs a spend policy inside the emit gate (paid calls, capped, logged),
a judge-versioning story, and judge-eval evidence before any of it is
honest. Recorded here, not built.

**Confidence intervals over agreement** (e.g. a Clopper–Pearson lower
bound ≥ threshold). Statistically stronger against small samples — 2/3 is
weak evidence about a population rate — but it changes the claim from
"measured agreement on these inputs" to an inference about unseen inputs,
which is exactly what this gate must not assert (nothing is extrapolated,
spec/contract.md §6). The measured rate over the actually-replayed inputs
is the honest v0 claim; an interval bound could arrive later as an
additional declared field without disturbing this one.

## consequences

- Divergent-reference signatures become compilable when — and only when —
  a contract author declares how much divergence the differential may
  absorb: an explicit, id-bearing choice, never a default.
- The manifest records the measured agreement next to the declared
  threshold; a reader can always distinguish "undeclared-exact" from
  "declared 800‰, measured 833‰". Truncation means a reported 99.9% is
  really ≥ 99.9% and < 100% — the number never flatters.
- `Some(1000)` is not a synonym for `None`: both demand total agreement,
  but they are different declarations (different ids), and `None` keeps
  v0's per-input hard-fail wiring bit-for-bit.
- The training side stays open: the gate can accept a divergent reference,
  but a trainer still needs a canonical witness pick per divergent input
  (which recorded output to learn from) — acceptance changes the verdict,
  not the data. Remains in spec/adr/open-questions.md.
- `Contract` gains a field; the only struct-literal constructors are test
  fixtures (this crate, auto-backend), updated at the wiring site.

## amendment — wave 7: training-side canonical pick

scope: `crates/auto-backend` (pure pick),
`crates/auto-cli` (explicit flag).

The consequence left open above — a trainer needs a canonical witness per
divergent input; acceptance changes the verdict, not the data — closes
behind an explicit operator flag:

1. **Pick rule.** `Recorded` gains `output_counts`: witness count per
   canonical output (`outputs` with multiplicity, populated by both gather
   paths — span and region). `canonical_pick` returns a group's
   most-witnessed canonical output, parsed back to a value.
2. **Tie rule.** Equal witness counts break toward the lexicographically
   smallest canonical string. Stated plainly: a 1–1 split trains on
   whichever output sorts first as canonical JSON — deterministic and
   order-independent, and arbitrary in exactly the way a tie is; the rule
   is written here rather than hidden in code.
3. **Errored-skip rule.** A group with any recorded error is never
   trainable: no pick, ever. `pick_observations` returns `(input, pick)`
   pairs in canonical input order plus the count of errored groups it
   skipped, so the CLI reports the skip instead of absorbing it.
4. **Honesty framing.** Training on the majority witness is an EXPLICIT
   operator choice — the CLI's `--divergent-pick most-common` — never a
   silent default: absent the flag, gather refuses divergent groups
   exactly as before, bit-for-bit. The flag selects training DATA only.
   The declared `differential_min_agreement_milli` remains the sole
   acceptance authority at the gate: a majority-trained subject still
   passes or fails on the measured agreement rate, and an undeclared-exact
   contract still hard-fails divergent references at the differential.

Still open (spec/adr/open-questions.md): per-class / weighted witness
sampling — one majority pick per input discards minority witnesses
entirely.
