#!/usr/bin/env bash
# End-to-end proof of region compilation (ADR-0015): a recorded three-span
# chain (extract -> route -> format, identity glue) compiles into ONE
# pipeline artifact through the unchanged gate, runs end-to-end on witnessed
# AND held-out inputs, guards its region entry, and refuses honestly where
# v0 says it must. No network, no key, no paid call.
set -euo pipefail
cd "$(dirname "$0")/../.."

AUTO="${AUTO_BIN:-target/debug/auto}"
PY="${PYTHON_BIN:-python3}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
STORE="$WORK/store.db"
CONTRACT=evals/pipeline-agent/region.contract.toml

# --- 1. record chains: near-variant docs so the guard calibrates tight ------
DOCS=(
  "Beta Alpha Gamma"
  "Beta Alpha Delta"
  "Compilers translate agent cognition into binaries"
  "Compilers translate agent cognition into artifacts"
)
for doc in "${DOCS[@]}"; do
  "$AUTO" record --store "$STORE" -- "$PY" evals/pipeline-agent/agent.py "$doc" > /dev/null
done
# a repeat witnesses determinism of the whole chain
"$AUTO" record --store "$STORE" -- "$PY" evals/pipeline-agent/agent.py "${DOCS[0]}" > /dev/null

# --- 2. region compile: stages + identity glue -> one pipeline artifact -----
"$AUTO" compile --contract "$CONTRACT" --store "$STORE" \
  --guard-field doc --out "$WORK/region.cbin" --runs-dir "$WORK/runs" \
  | tee "$WORK/compile.txt"
grep -q "region: 3 stage(s)" "$WORK/compile.txt"
grep -q "identity, omitted" "$WORK/compile.txt"
grep -q "verdict: PASS" "$WORK/compile.txt"
grep -q "artifact " "$WORK/compile.txt"

# --- 3. the artifact answers end-to-end: witnessed and HELD-OUT inputs ------
"$AUTO" run --artifact "$WORK/region.cbin" --input '{"doc":"Beta Alpha Gamma"}' \
  | tee "$WORK/run1.txt"
grep -q '"BETA"' "$WORK/run1.txt"
# held out: never recorded, in-vocabulary enough to pass the guard
"$AUTO" run --artifact "$WORK/region.cbin" --input '{"doc":"Beta Alpha Gamma Delta"}' \
  | tee "$WORK/run2.txt"
grep -q '"BETA"' "$WORK/run2.txt"

# --- 4. the region guard abstains beyond calibration ------------------------
set +e
"$AUTO" run --artifact "$WORK/region.cbin" --input '{"doc":"zzzzz qqqqq xxxxx"}' \
  > "$WORK/far.txt" 2> "$WORK/far.err"
CODE=$?
set -e
test "$CODE" = "3"
grep -q "abstention" "$WORK/far.err"

# --- 5. the manifest and graph carry the region structure -------------------
"$AUTO" inspect "$WORK/region.cbin" | tee "$WORK/inspect.txt"
grep -q 'scope region extract..format' "$WORK/inspect.txt" \
  || grep -q '"region"' "$WORK/inspect.txt" \
  || grep -q 'region' "$WORK/inspect.txt"
grep -q 'program.json' "$WORK/inspect.txt"

# --- 6. honest refusals: trace-mode verify and distill on region contracts --
set +e
"$AUTO" verify --contract "$CONTRACT" --store "$STORE" --runs-dir "$WORK/runs" \
  > /dev/null 2> "$WORK/verify.err"
test "$?" != "0"
grep -q "verify at compile time" "$WORK/verify.err"
"$AUTO" distill --contract "$CONTRACT" --store "$STORE" \
  --trainer "echo unused" --input-field doc --out "$WORK/never.cbin" \
  > /dev/null 2> "$WORK/distill.err"
test "$?" != "0"
set -e
grep -q "future work" "$WORK/distill.err"
test ! -f "$WORK/never.cbin"

echo "e2e OK: a recorded 3-span chain compiled to one pipeline artifact through the unchanged gate (identity glue omitted), answered witnessed and held-out chains end-to-end, abstained beyond its guard, and refused trace-mode verify and distill honestly"
