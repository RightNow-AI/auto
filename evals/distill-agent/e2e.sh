#!/usr/bin/env bash
# End-to-end proof of the S5 distillation pipeline: record one run per corpus
# document through the real python SDK, prove the extraction DSL honestly
# CANNOT express the routing rule (budget exhaustion, nonzero exit), then
# distill a decision tree through the SAME emit gate and run it on held-out
# documents. Requires python with scikit-learn (the trainer). No network.
set -euo pipefail
cd "$(dirname "$0")/../.."

AUTO="${AUTO_BIN:-target/debug/auto}"
PY="${PYTHON_BIN:-python3}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
STORE="$WORK/store.db"

# --- 1. one recorded run per corpus line (36 docs; stdout to a log for CI) --
while IFS= read -r doc; do
  doc="${doc%$'\r'}" # tolerate CRLF checkouts
  "$AUTO" record --store "$STORE" -- "$PY" evals/distill-agent/agent.py "$doc" \
    >> "$WORK/record.log"
done < evals/distill-agent/corpus.txt

# --- 2. sanity: all 36 runs landed in the store ----------------------------
"$AUTO" report --task distill-agent --store "$STORE" | tee "$WORK/report.txt"
grep -q '36 traces' "$WORK/report.txt"

# --- 3. extraction alone must FAIL: the rule needs contains + branching, ---
# which the closed DSL cannot spell; exhaustion is the honest outcome
# (searches the full budget, 300k states — takes a minute or two)
if "$AUTO" compile --contract evals/distill-agent/router.contract.toml \
  --store "$STORE" --out "$WORK/x.cbin" --runs-dir "$WORK/runs" \
  > "$WORK/extract-out.txt" 2> "$WORK/extract-err.txt"; then
  echo "ERROR: enumerative extraction claimed the fuzzy rule — it must refuse"
  exit 1
fi
grep -q 'synthesis budget exhausted' "$WORK/extract-err.txt"
test ! -f "$WORK/x.cbin"

# --- 4. distill: external sklearn trainer, SAME emit gate as compile --------
"$AUTO" distill --contract evals/distill-agent/router.contract.toml \
  --store "$STORE" --trainer "$PY crates/auto-passes/trainer/tree_train.py" \
  --input-field text \
  --out "$WORK/router.cbin" --runs-dir "$WORK/runs" | tee "$WORK/distill.txt"
grep -q 'distilled:' "$WORK/distill.txt"
grep -q 'holdout_accuracy' "$WORK/distill.txt"
grep -q 'verdict: PASS' "$WORK/distill.txt"
grep -q 'artifact ' "$WORK/distill.txt"

# --- 5. held-out documents (none appear in corpus.txt), one per class ------
"$AUTO" run --artifact "$WORK/router.cbin" \
  --input '{"text":"A breach just hit the payments api."}' \
  | tee "$WORK/run-urgent.txt"
grep -q '"urgent"' "$WORK/run-urgent.txt"

"$AUTO" run --artifact "$WORK/router.cbin" \
  --input '{"text":"The retro notes went to the wiki and the crew added owners for every item."}' \
  | tee "$WORK/run-long.txt"
grep -q '"long"' "$WORK/run-long.txt"

"$AUTO" run --artifact "$WORK/router.cbin" \
  --input '{"text":"Sure, noon works for me."}' \
  | tee "$WORK/run-short.txt"
grep -q '"short"' "$WORK/run-short.txt"

"$AUTO" inspect "$WORK/router.cbin" | tee "$WORK/inspect.txt"
grep -q 'distilled' "$WORK/inspect.txt"
grep -q 'program.json' "$WORK/inspect.txt"

echo "e2e OK: 36 runs recorded, extraction refused honestly, distilled tree passed the gate and routed 3 held-out docs"
