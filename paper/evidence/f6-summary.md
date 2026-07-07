# F6 novelty-stream - the ratchet curve (H1)

Source: `paper/evidence/f6-ratchet-a400-agent.csv` (300 positions). Frozen shifts: 50 (+security),
120 (+onboarding), 200 (+billing-fraud phrasing). Window = 25 positions.

## per-window decay

| window | positions | mean cost u$/item | mean latency ms | tier-1 % | deopt | bootstrap | new distinct | cost bar |
|---|---|---|---|---|---|---|---|---|
| 1-25 | 25 | 18.9 | 389 | 68% | 0 | 8 | 8 | `########################` |
| 26-50 <- shift 1 @50: +security | 25 | 2.5 | 125 | 96% | 0 | 0 | 1 | `###` |
| 51-75 | 25 | 19.3 | 474 | 68% | 0 | 0 | 5 | `########################` |
| 76-100 | 25 | 16.7 | 535 | 72% | 0 | 0 | 3 | `#####################` |
| 101-125 <- shift 2 @120: +onboarding | 25 | 6.9 | 311 | 88% | 0 | 0 | 2 | `#########` |
| 126-150 | 25 | 14.1 | 393 | 76% | 0 | 0 | 3 | `##################` |
| 151-175 | 25 | 4.5 | 337 | 92% | 0 | 0 | 2 | `######` |
| 176-200 <- shift 3 @200: +billing-fraud phrasing | 25 | 2.3 | 317 | 96% | 0 | 0 | 1 | `###` |
| 201-225 | 25 | 7.2 | 391 | 88% | 0 | 0 | 2 | `#########` |
| 226-250 | 25 | 14.1 | 647 | 76% | 0 | 0 | 1 | `##################` |
| 251-275 | 25 | 2.3 | 169 | 96% | 0 | 0 | 0 | `###` |
| 276-300 | 25 | 2.4 | 183 | 96% | 0 | 0 | 0 | `###` |

cost/item sparkline (one char per window, scaled to the peak window):

    [@.@%-#:.-#..]  peak = 19.3 u$/item

## recompile events

| pos | event | generation | distinct witnesses | detail |
|---|---|---|---|---|
| 8 | compile-pass | 1 | 8 | eval_run=c95ac45da795fca8fe622761e1c53f2634c9023b97939c8faa2880f78bd8959e wall_ms=1859 |
| 83 | compile-pass | 2 | 16 | eval_run=64534ee80d600a4b6413e0ccfbe4cfad2c5c0bbb835c446bb4ce7253c6cb8921 wall_ms=1975 |
| 154 | compile-pass | 3 | 24 | eval_run=2164383c1b87b591f24d88342d21b93e72ec73bab986430b45be94d39bac295b wall_ms=2292 |

## totals

- ratchet leg total: **2775 u$** over 300 positions (47 paid calls = 8 bootstrap + 39 agent-deopt re-records, 253 tier-1 answers, 0 errors/abstains)
- arithmetic control total (every position bought at its measured tier-0 price): **17692 u$** (218 positions estimated from the mean; `paper/evidence/f6-control.csv` (driver --control output))
- control / ratchet cost ratio: **6.4x**

## honesty notes

- The control is ARITHMETIC (rule in this file's docstring): `auto run` has no
  artifact-less tier-0 mode, so the flat-cost line is computed from the measured
  per-ticket tier-0 prices, never re-fired as a second paid pass.
- tier-1 rows measure a one-shot `auto run` wall time (process spawn + wasm compile
  included). The resident runner (spec/runtime.md par.9) amortizes that; this leg
  reports the honest one-shot number.
- Guards are lexical (trigram distance, spec/runtime.md par.2): a tier-1 hit means
  in-calibration, not verified-correct. Correctness under distribution shift is
  H4's measurement (false-proceed rate), not this CSV's.
- Tier-0 deopt answers are unverified reference authority folded in by the next
  recompile gate; refused/inconclusive recompiles appear in the events table and
  their cost stays in the curve - failures are results.
