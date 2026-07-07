# ADR-0021: judged differential — the judge arbitrates byte-divergent replay groups, the declared threshold still decides

status: accepted · scope: `crates/auto-contract` (format
+ arbiter semantics), `crates/auto-backend` (feeds the arbiter from the
differential gate), `crates/auto-cli` (threads the judge into the
differential), `spec/contract.md` §2.

## context

The wave-8 judged seam (ADR-0019) covers **examples only**: differential
rows still demand byte-equal reproduction of the recorded output. So a
paraphrasing-but-faithful subject fails them, no matter what a judge would
say. The motivating residue is wave 5's `summarize` signature — 20%
witnessed-deterministic free text: ADR-0018's statistical acceptance can
price in the divergence (divergent references count unmatched), but the
ceiling for an honest subject is then the deterministic fraction itself;
the 80% that paraphrases can never count as matched under byte equality.
ADR-0019 recorded the judged differential explicitly as future work, not
built, pending a budget-honest design. This ADR closes it by composing the
two accepted pieces: ADR-0018's statistical acceptance (what decides) and
ADR-0019's judge seam (what counts as matched).

## decision

1. **One declared key.** `[acceptance]` gains `differential_match`:
   `"exact"` (default) or `"judged"`; anything else is a loud parse error
   naming the two values. `Acceptance` gains
   `differential_match: DifferentialMatch` (`Exact | Judged`, default
   `Exact`). `"judged"` **requires** a declared
   `differential_min_agreement_milli` — rejected loudly without one: the
   ADR-0018 threshold is the sole acceptance authority, and a judged
   differential with no threshold would have no decision rule. Declaring
   the relaxation therefore always means declaring both halves: what counts
   as matched (judged) and how much must match (the threshold).
2. **Exact is byte-identical to today.** `differential_match = "exact"`
   (or absent) changes nothing: the existing pinned canonical-json and
   differential tests pass unmodified, and no new code path can consult a
   judge under it.
3. **Judged semantics, in order.** For each replayed differential group
   (one distinct recorded input, canonical input order):
   - subject output byte-equal (canonical json) to the group reference ⇒
     counts **matched without consulting the judge** — the wave-8 free
     short-circuit; per-group byte-equal lines keep the exact-mode shape;
   - byte-divergent ⇒ consult `judge.equivalent(reference, subject_output,
     task_context)` **once**, `task_context` pinning the group:
     `task "<task>", differential input #<i>`. Judged-yes counts matched
     and its evidence line says **JUDGED equivalent** — never silently
     identical to a byte match; judged-no counts unmatched and its line
     says **JUDGED not equivalent** with both values;
   - judge `Err` ⇒ the whole agreement check is **Failed** with the error
     in its detail — a judge failure never passes and never quietly counts
     as a mere mismatch; later divergent groups are not consulted (each
     consult may be a paid call and the count is already unusable);
   - **no judge supplied** ⇒ the agreement check is **Unchecked** with
     detail `judged differential declared but no judge supplied (pass
     --judge-model)` → verdict Inconclusive, never Pass. It never falls
     back to exact counting — not even when every group is byte-equal: the
     declaration demands the instrument. Per-group byte-equal evidence
     lines still appear;
   - groups with recorded errors (no trustworthy reference; subject not
     run) and groups the subject errored on count **unmatched** — exactly
     the shortfall the declared threshold prices in; neither is a judge
     matter.
   The declared `differential_min_agreement_milli` then decides via the
   unchanged ADR-0018 math: `matched × 1000 ≥ milli × eligible`, matched =
   byte-equal + judged-equivalent, eligible = every group.
4. **Reference for divergent-reference groups = the ADR-0018 canonical
   pick.** When a group's recorded outputs themselves diverge, the judged
   differential compares (and, if needed, judges) against the group's
   majority witness, ties broken toward the lexicographically smallest
   canonical string — the SAME `canonical_pick` the ADR-0018 amendment
   defined for training; no second pick rule exists. Every line over a
   picked reference says so (`reference is the ADR-0018 majority pick over
   N distinct recorded outputs`). Groups with any recorded error still have
   no reference, ever.
5. **Seam placement.** The arbiter —
   `harness::judged_differential_checks(task, comparisons, min_milli,
   judge)` over per-group `harness::DifferentialComparison` values — lives
   in `auto-contract` beside `agreement_check` and the `Judge` trait, so
   its semantics are pinned hermetically with `ScriptedJudge`; the
   differential gate (`auto-backend`) prepares the comparisons (it owns
   replay, grouping, and the pick) and the judge implementation stays
   outside with the spend rails (ADR-0010: capped, ledgered, priced —
   this crate never spends). Judge calls ride the same session cap as
   every other frontier call.
6. **Judge call budget.** At most **one judge call per byte-divergent
   distinct input** — byte-equal groups are free, duplicate observations
   of one input are one group, and a judge failure stops further consults.
   An operator can bound gate spend by the number of distinct recorded
   inputs before running.
7. **Id-bearing.** `differential_match` flows into the canonical
   contract-id JSON exactly the way `differential_min_agreement_milli`
   does (the canonical `acceptance` table, absent keys omitted): relaxing
   the differential changes what the contract IS, so `"judged"` gets a
   different id and can never masquerade under the exact contract's id —
   pinned by test. One asymmetry, deliberate: declared `"exact"` is NOT
   emitted (same id as absent). The field is non-optional with default
   `Exact`, so declared-exact and undeclared-exact are the same claim
   stated twice — unlike the threshold, where ADR-0018 chose to
   distinguish `Some(1000)` from `None` because the two produce different
   check wiring. Here exact-declared and exact-absent produce bit-for-bit
   identical behavior, and an id must change only when the normative
   claims do. `contract_version` stays 0 by ADR-0018's rule: every
   released strict parser rejects the new key loudly, never misreads it.

## alternatives considered

**Embedding similarity** (cosine over output embeddings ≥ threshold).
Cheaper and locally runnable, but already rejected for examples in
ADR-0019 and worse here: a population-level acceptance built on a distance
that cannot distinguish a faithful paraphrase from a subtle contradiction
would let exactly the wrong 20% through. Could still arrive later as a
declared, calibrated property — not as the meaning of `differential_match`.

**Judge every row** (consult on every replayed observation, byte-equal or
not). Strictly more judge evidence, and strictly dishonest about cost:
byte equality needs no opinion, and each consult is a paid call. The
wave-8 short-circuit principle holds — equal pairs are never sent.

**Per-observation instead of per-group judging** (judge each recorded
witness of an input separately, accept if any/most are equivalent). The
differential's unit of claim is the distinct input, and ADR-0018 already
fixed both the counting unit (groups) and the reference for divergence
(the canonical pick). Judging minority witnesses multiplies paid calls by
recorded multiplicity to answer a question the gate does not ask. The real
gap it gestures at — minority witnesses are never consulted at all — is
recorded in open-questions, not solved by burning more calls here.

## consequences

- The wave-5 summarize residue becomes compilable honestly: a subject that
  paraphrases faithfully can clear a declared threshold, with every judged
  group marked JUDGED and the measured rate recorded — the manifest reader
  can always distinguish byte agreement from judged agreement.
- A judged differential is doubly declared (mode + threshold), id-bearing,
  and never a default. No judge, no pass: gates must be given
  `--judge-model` or the verdict is Inconclusive, which blocks emit by the
  existing rule.
- Judge verdicts are opinions: re-running may flip a borderline group, so
  judged-differential eval runs are reproducible only up to the judge.
  Accepted and stated (ADR-0019 said the same for examples); the eval run
  records which judge said so.
- `Acceptance` gains a field; the only struct-literal constructors are
  this crate's own parse/tests — every other construction site already
  uses `Default`.
