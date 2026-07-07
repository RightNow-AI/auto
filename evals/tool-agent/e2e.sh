#!/usr/bin/env bash
# End-to-end proof of CAPABILITY IMPORTS (ADR-0017): a recorded chain whose
# middle span is a real tool call compiles into an artifact that DECLARES
# the tool as a capability and imports auto.tool_call. The gate replays the
# recorded tool pairs hermetically (no live tool in CI verification); a live
# run provides the tool with --tool; a run WITHOUT the tool refuses; the
# guard still abstains beyond calibration. No network anywhere.
set -euo pipefail
cd "$(dirname "$0")/../.."

AUTO="${AUTO_BIN:-target/debug/auto}"
PY="${PYTHON_BIN:-python3}"
WORK="target/tool-agent-e2e-$$"
rm -rf "$WORK"; mkdir -p "$WORK"
trap 'rm -rf "$WORK" 2>/dev/null || true' EXIT
STORE="$WORK/store.db"
CONTRACT=evals/tool-agent/region.contract.toml
TOOL="lookup=$PY evals/tool-agent/lookup.py"

# --- 1. record near-variant chains ------------------------------------------
DOCS=(
  "Beta Alpha Gamma"
  "Beta Alpha Delta"
  "Compilers translate agent cognition into binaries"
  "Compilers translate agent cognition into artifacts"
)
for doc in "${DOCS[@]}"; do
  "$AUTO" record --store "$STORE" -- "$PY" evals/tool-agent/agent.py "$doc" > /dev/null
done
"$AUTO" record --store "$STORE" -- "$PY" evals/tool-agent/agent.py "${DOCS[0]}" > /dev/null

# --- 2. region compile: the tool span becomes a declared capability ---------
"$AUTO" compile --contract "$CONTRACT" --store "$STORE" \
  --guard-field doc --out "$WORK/tool.cbin" --runs-dir "$WORK/runs" \
  | tee "$WORK/compile.txt"
grep -q "capabilities: lookup" "$WORK/compile.txt"
grep -q "verdict: PASS" "$WORK/compile.txt"
grep -q "artifact " "$WORK/compile.txt"

# --- 3. the manifest carries the capability ---------------------------------
"$AUTO" inspect "$WORK/tool.cbin" | tee "$WORK/inspect.txt"
grep -q "lookup" "$WORK/inspect.txt"

# --- 4. a run WITHOUT the tool refuses, naming the missing capability -------
set +e
"$AUTO" run --artifact "$WORK/tool.cbin" --input '{"doc":"Beta Alpha Gamma"}' \
  > /dev/null 2> "$WORK/refuse.err"
CODE=$?
set -e
test "$CODE" != "0"
grep -q "lookup" "$WORK/refuse.err"
grep -q "tool host" "$WORK/refuse.err"

# --- 5. a run WITH the tool answers, witnessed AND held-out ------------------
"$AUTO" run --artifact "$WORK/tool.cbin" --input '{"doc":"Beta Alpha Gamma"}' \
  --tool "$TOOL" | tee "$WORK/run1.txt"
grep -q '"TEAM-B"' "$WORK/run1.txt"
"$AUTO" run --artifact "$WORK/tool.cbin" --input '{"doc":"Beta Alpha Gamma Zeta"}' \
  --tool "$TOOL" | tee "$WORK/run2.txt"
grep -q '"TEAM-B"' "$WORK/run2.txt"

# --- 6. the guard still abstains beyond calibration --------------------------
set +e
"$AUTO" run --artifact "$WORK/tool.cbin" --input '{"doc":"zzzzz qqqqq xxxxx"}' \
  --tool "$TOOL" > /dev/null 2> "$WORK/far.err"
CODE=$?
set -e
test "$CODE" = "3"
grep -q "abstention" "$WORK/far.err"

echo "e2e OK: a recorded tool-calling chain compiled to a capability artifact (auto.tool_call imported, lookup declared), the gate replayed the tool hermetically, a toolless run refused naming the capability, a --tool run answered witnessed and held-out chains, and the guard abstained beyond calibration"
