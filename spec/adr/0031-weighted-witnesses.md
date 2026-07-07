# ADR-0031: weighted witness training — the full recorded distribution, weight = witness count

status: accepted · scope: `crates/auto-backend` (pure
weighted rows), `crates/auto-passes/trainer` (weight-aware trainers +
tests), `crates/auto-cli` (explicit flag value, distill only),
`spec/distillation.md` §4.1.

## context

The ADR-0018 amendment gave training under divergence one shape:
`--divergent-pick most-common` trains on ONE canonical pick per divergent
group and discards every minority witness. The open-questions ledger
recorded the gap immediately ("per-class / weighted witness sampling
instead of one majority pick"). A reference that answers `x` six times and
`y` once is evidence about a distribution; collapsing it to `x` erases the
measured disagreement before the trainer ever sees it.

## decision

1. **Weighted rows are a pure function.**
   `auto_backend::differential::weighted_observations(&Gathered) ->
   (Vec<(input, output, weight)>, errored_skipped)`: per non-errored group,
   one row per **distinct witnessed output**, weight = that output's
   witness count (`output_counts`, the ADR-0018-amendment evidence). Groups
   in canonical input order, outputs in canonical order within a group —
   deterministic and order-independent. Errored groups are skipped and
   counted, never trainable: exactly `pick_observations`' rule.
2. **The trainer wire extends back-compatibly.** The observation JSONL
   gains an OPTIONAL `"weight"` per line: a JSON integer ≥ 1, absent = 1.
   The emitter omits `weight` when it is 1, so weightless runs produce
   byte-identical files; the parsers reject 0, negatives, floats, bools,
   and strings loudly. A file whose weights are all 1 **is** the weightless
   protocol: same fit call, same metrics line, byte-for-byte (pinned by
   test). Weight-blind entrypoints (`parse_observations`, kept for the
   modal wrappers) REFUSE a file carrying any weight ≠ 1 instead of
   silently dropping witness counts.
3. **Conflicting labels for one input are the point.** The trainer sees
   the true witnessed distribution and resolves it by weight: sklearn
   `sample_weight` for the tree (split criterion and leaf argmax count
   witness mass), per-example loss weighting for the mlp
   (`(ce_i·w_i).Σ / w.Σ` — the mean loss of the witness-expanded batch
   without materializing repeats).
4. **Metrics stay honest, holdout stays unweighted.** When any weight > 1
   the metrics line adds `weighted_train_accuracy` (witness mass reproduced
   over total training mass — the objective the fit saw) and `train_weight`
   (that total). `train_accuracy` remains the PLAIN fraction of training
   rows — under divergence 100% is impossible by construction and the
   shortfall is the recorded disagreement, reported. `holdout_accuracy`
   remains PLAIN UNWEIGHTED accuracy on held-out rows: measured reality,
   not the training trick. `TrainerMetrics` has `deny_unknown_fields` off
   by design, so the added fields break nothing.
5. **THE GATE IS UNCHANGED.** `--divergent-pick weighted` selects training
   DATA only, exactly like `most-common`: the differential replay and the
   declared `differential_min_agreement_milli` (ADR-0018) remain the sole
   acceptance authority. A weighted-trained subject reproduces whichever
   output it learned; divergent groups still count against agreement per
   the ADR-0018/0021 rules; an undeclared-exact contract still hard-fails
   divergent references. Nothing about weighting can make an emit easier —
   it can only change which behavior faces the same gate.
6. **Distill-only.** `weighted` is a `--divergent-pick` value on
   `auto distill` alone. Synthesis rejects conflicting observations by
   construction (same input, different outputs is a CEGIS refusal), so
   `auto compile` refuses the value loudly instead of accepting a flag it
   cannot honor.

## where weighted and most-common differ — measured

Per group they agree: a weighted fit over one group's rows resolves to the
group's heaviest output — the same argmax over the same counts as
`canonical_pick`, same tie rule (asserted in
`crates/auto-backend/tests/divergence.rs`). They differ where the frozen
features cannot separate groups (colliding trigrams, texts under 3 chars,
depth limits): one leaf then holds several groups' rows, and most-common
counts **group votes** where weighted counts **witness mass**.

Measured fixture (`trainer/test_trainers.py::
test_weighted_vs_most_common_yield_different_trained_behavior`, sklearn
1.9.0, seed 0): three inputs whose texts featurize identically (all under
3 chars → zero feature vectors → a single leaf); witnesses `aa`: x×6 y×1,
`bb`: y×2, `cc`: y×2 — 11 witnesses, x 6 / y 5.

| training data | leaf counts | trained answer (all 3 inputs) | group majorities reproduced | witness mass reproduced |
|---|---|---|---|---|
| most-common (3 rows) | x:1 y:2 | `y` | 2/3 | 5/11 |
| weighted (4 rows) | x:6 y:5 | `x` | 1/3 | 6/11 |

Neither dominates: most-common maximizes group-vote agreement, weighted
maximizes witness-mass agreement. The differential gate prices whichever
is emitted per replayed input against the declared threshold — the choice
is measured, never assumed. The single-group flip is also pinned: rows
x,x,y(weight 5) train to `x` plain and `y` weighted (tree), and a 1-vs-9
mlp weighting flips with its mirror, same seed.

## alternatives considered

**Row repetition** (expand weight n into n identical rows; no format
change). Identical tree criterion math for integer weights, but repeated
rows leak across the holdout split (identical copies in train and holdout
flatter `holdout_accuracy`), `train_n` stops meaning distinct rows, and
the mlp batch grows with witness counts. Rejected: the weight field is one
integer and keeps every honest number honest.

**Per-class balanced sampling** (the other half of the open-questions
phrase). Reweighting toward class balance trains toward a distribution
nobody witnessed; the honest v0 target is the distribution as recorded.
Class balancing remains open as a deliberately different choice.

**Distribution-calibrated heads** (train to predict the witness
distribution, not a label). The frozen wire formats emit one label;
probability heads are a `model_version`/`mlp_version` bump and a different
artifact contract. Recorded, not built.

**Fractional or normalized weights.** Witness counts are integers;
nothing normative here needs floats (kin to ADR-0014's integer-milli
rule). A future importance-weighting scheme can extend the field's domain
under its own ADR.

## consequences

- Minority witnesses reach the trainer for the first time; where features
  collide, trained behavior measurably differs from most-common (table
  above) — an operator choosing between them is choosing which agreement
  to maximize, and the same gate measures either.
- Weightless everything is bit-for-bit unchanged: absent flag → refusal,
  `most-common` → unchanged, all-ones weighted file → byte-identical model
  and metrics (pinned by test on both trainers).
- The modal wrappers stay weight-blind and refuse loudly; threading
  weights through them is a small change when remote weighted training is
  wanted.
- The verification-side twin stays open (ADR-0021's recorded gap):
  minority witnesses are now trainable but remain invisible to the judged
  differential's verdict, which compares one reference per group.
- Still open (spec/adr/open-questions.md): per-class sampling,
  distribution-calibrated heads.
