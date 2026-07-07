# ADR-0019: judged matching — per-example LLM-judged semantic equivalence behind a trait seam

status: accepted · scope: `crates/auto-contract` (format
+ `Judge` seam + harness semantics), `crates/auto-cli` (wires the frontier
judge into the gate), `spec/contract.md` §2/§6.

## context

The last recorded contract gap ("Judged / statistical match modes",
spec/adr/open-questions.md): generative outputs need judged comparison —
`match = "exact"` was the only mode. ADR-0018 closed the statistical half
(declared differential agreement) and explicitly recorded the judged half as
"recorded here, not built": a judge needs a spend policy inside the emit
gate before any of it is honest. That policy now exists — ADR-0010's capped
client, append-only ledger, and pinned price table are live and every paid
call in the workspace already rides them. The motivating residue is wave 5's
`summarize` signature (20% deterministic): a compiled subject can produce a
semantically faithful summary that will never byte-match a recorded one, so
no normative example over it can ever pass exactly.

## decision

1. **Per-example mode, exact stays the plain case.** `MatchMode` gains
   `Judged`; `match = "judged"` parses, anything else still rejects loudly.
   Exact remains the default posture and the only mode that needs no judge.
   Judged examples ask a judge whether subject output and expected output
   are **semantically equivalent for the contracted task** — never whether
   the output is merely plausible.
2. **Trait seam in the contract crate; spend stays outside.** `harness::
   Judge { equivalent(expected, actual, task_context) -> Result<bool,
   String>; describe() }`, consumed by `verify_against_subject_with_judge
   (contract, subject, Option<&mut dyn Judge>)`. The frontier judge
   implementation lives with the spend rails (ADR-0010: capped, ledgered,
   priced) outside this crate — `auto-contract` never spends and its tests
   stay hermetic, scripted through the seam (`ScriptedJudge`, a labeled test
   fake mirroring `ScriptedFrontier`). `verify_against_subject` delegates
   with `None`; judge-less behavior is byte-identical to v0.
3. **Verdict semantics, in order.** Exactly-equal outputs (canonical-json)
   pass **without consulting the judge** — a free short-circuit, noted in
   the detail; each judge consult may be a paid call, so equal pairs are
   never sent. Divergent output + no judge = **Unchecked** ("judged match
   declared but no judge supplied (pass --judge-model)") → Inconclusive,
   never Pass. Judge says not-equivalent, or the judge call errors =
   **Failed** — a judge failure never passes. Exact examples never touch
   the judge.
4. **Judged examples only in v0.** The differential reproduction claim
   stays exact-or-statistical (ADR-0018); a judged DIFFERENTIAL — judge
   calls over every divergent replayed input — is recorded future work, not
   built: it multiplies paid calls by the input population and needs its
   own budget declaration before it is honest.
5. **Id-bearing automatically.** `match` was already part of the canonical
   example JSON (`example_json` emits `"match":"exact"`), so `Judged`
   flows into the contract-id preimage with no canonicalization change —
   verified by test: exact and judged versions of the same example get
   different ids, and every existing exact contract keeps its id bit-for-
   bit. A relaxed comparison claim can never masquerade under an exact
   contract's id. `contract_version` stays 0 by ADR-0018's rule: the strict
   parser makes old readers reject `"judged"` loudly, never misread it.

## alternatives considered

**Embedding similarity** (cosine over output embeddings ≥ threshold).
Cheaper per comparison and locally runnable, but the threshold is a number
pretending to be a meaning: it needs per-task calibration evidence to claim
"equivalent", and a near-miss summary and a subtle contradiction can sit at
the same distance. Could arrive later as a declared, calibrated property —
not as the meaning of `match`.

**Exact-only forever.** Honest and free, but it permanently excludes
normative examples over generative outputs — the wave-5 summarize residue
stays uncompilable even when a judge would accept it, and authors start
encoding fake exactness (trimmed, canonicalized reference strings) to
smuggle fuzziness past the gate.

**Judge inside the contract crate.** One crate, no seam — but it drags paid
HTTP, the cap, and the ledger into the format crate; contract tests would
need network or mocks-pretending-to-be-models, both forbidden. The seam
keeps the format crate pure and the spend rails singular (ADR-0010: one
crate is the only paid path).

## consequences

- A judge is a model with opinions. Every check that rests on a judge
  verdict says **JUDGED** in its detail and names the judge
  (`Judge::describe`), so no reader mistakes it for exact reproduction;
  the free short-circuit says the outputs were exactly equal and the judge
  was not consulted. The distinction survives into eval run records.
- A contract with judged examples cannot Pass without a judge: gates and
  `auto verify` must be given one (`--judge-model`) or the verdict is
  Inconclusive — which blocks emit, by the existing rule.
- Judge calls are paid, capped, and ledgered (ADR-0010); the judge rides
  the same session cap as every other frontier call. Divergent outputs are
  deduplicated canonically before consulting — the same pair is never
  judged twice in one run.
- Judge verdicts are not reproducible the way exact checks are: re-running
  may flip a borderline call. Accepted and stated — the eval run records
  which judge said so. Judge versioning/eval evidence beyond naming stays
  open (spec/adr/open-questions.md).
- Trace-mode verification (`verify_against_store`) carries no judge in v0:
  a judged example whose recorded outputs diverge is Unchecked there.
