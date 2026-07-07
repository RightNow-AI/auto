# paper/ — evidence log for the eventual paper

This directory is NOT the paper. It is the running, honest record every
future claim must trace back to: a claims ledger, a dated experiment log,
and checked-in evidence artifacts (eval runs, metrics lines, sweep tables).

Rules (constitution §honesty, applied to prose):

- a claim enters `claims.md` as **pending** until a measurement backs it;
  measured claims cite an eval run id, a commit, and a reproduction command.
- numbers from toy fixtures are labeled as such — they prove plumbing,
  not the thesis.
- refuted or weakened claims stay in the ledger with their refutation;
  nothing is silently deleted.
- `log.md` is append-only, dated, and records failures with the same care
  as successes (the wave-1 hyperparameter refusal is paper material).

When the paper is written, it is assembled FROM this directory; nothing is
claimed in it that is not already here.
