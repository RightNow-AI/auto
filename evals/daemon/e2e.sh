#!/usr/bin/env bash
# End-to-end proof of the ratchet AS A SERVICE (ADR-0013): a guard trip
# deopts to tier-0 and ingests; `auto daemon --once` notices the grown
# evidence, reruns the REAL compile gate, and publishes the recompiled
# artifact to the registry — the once-novel input then answers tier-1 with
# no human in the loop. Also proves the split-conformal guard flag
# (ADR-0014) end-to-end. No network, no key, no paid call.
set -euo pipefail
cd "$(dirname "$0")/../.."

AUTO="${AUTO_BIN:-target/debug/auto}"
PY="${PYTHON_BIN:-python3}"
# RELATIVE scratch dir on purpose: the --recompile string embeds paths
# inside ONE argument, and Git Bash's msys layer rewrites colon-bearing
# absolute paths there (C:/… reads as a path list). Relative paths carry no
# colon, so they survive on every host; the daemon resolves them against
# its cwd (the repo root).
WORK="target/daemon-e2e-$$"
rm -rf "$WORK"
mkdir -p "$WORK"
trap 'rm -rf "$WORK" 2>/dev/null || true' EXIT
STORE="$WORK/store.db"
REG="$WORK/reg"
CONTRACT=evals/toy-agent/fake-frontier.contract.toml

# --- 1. record near-variant docs (tight guard), compile v1, publish --------
DOC_B="Compilers translate agent cognition into fast deterministic binaries."
DOC_C="The quick brown fox naps over the lazy dog near the riverbank."
DOC_D="Compilers translate agent cognition into fast deterministic artifacts."
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py "$DOC_B" > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py "$DOC_B" > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py "$DOC_C" > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py "$DOC_D" > /dev/null

"$AUTO" compile --contract "$CONTRACT" --store "$STORE" \
  --guard-field prompt --out "$WORK/v1.cbin" --runs-dir "$WORK/runs" > /dev/null
"$AUTO" registry keygen --registry "$REG" > /dev/null
"$AUTO" registry add "$WORK/v1.cbin" --registry "$REG" > /dev/null
test "$("$AUTO" registry list --registry "$REG" | wc -l)" = "1"

# --- 1b. the conformal calibration flag reaches the wire (ADR-0014) --------
"$AUTO" compile --contract "$CONTRACT" --store "$STORE" \
  --guard-field prompt --guard-alpha-milli 100 \
  --out "$WORK/conformal.cbin" --runs-dir "$WORK/runs" | tee "$WORK/conformal.txt"
grep -q "split-conformal alpha 0.100" "$WORK/conformal.txt"
"$AUTO" run --artifact "$WORK/conformal.cbin" \
  --input '{"prompt":"The quick brown fox jumps over the lazy dog near the riverbank."}' \
  | grep -q '"brown jumps quick"'

# --- 2. novelty: guard trips, deopt answers via tier-0, evidence ingests ----
FAR='{"prompt":"zzzzz qqqqq xxxxx"}'
"$AUTO" run --artifact "$WORK/v1.cbin" --input "$FAR" \
  --tier0 "$PY evals/toy-agent/tier0_oracle.py" --store "$STORE" \
  > "$WORK/deopt.out" 2> "$WORK/deopt.err"
grep -q "observation ingested" "$WORK/deopt.err"

# --- 3. the daemon closes the loop with no human ----------------------------
# (within-process idempotence — a second cycle at the same count is a no-op —
# is proven by auto-daemon's own tests; a second --once PROCESS recompiles
# once redundantly by design: the watermark is in-memory, and a fresh compile
# is not byte-identical anyway — its manifest pins a fresh eval-run id. A
# persistent watermark is the recorded upgrade, ADR-0013.)
"$AUTO" daemon --once --store "$STORE" --contract "$CONTRACT" --registry "$REG" \
  --recompile "$AUTO compile --contract $CONTRACT --store $STORE --guard-field prompt --runs-dir $WORK/runs --out {out}" \
  2> "$WORK/daemon.err"
test "$("$AUTO" registry list --registry "$REG" | wc -l)" = "2"

# --- 4. the once-novel input answers tier-1 on the recompiled artifact ------
V1_ID=$("$AUTO" inspect "$WORK/v1.cbin" | sed -n 's/^artifact \([0-9a-f]*\).*/\1/p')
NEW_ID=$("$AUTO" registry list --registry "$REG" | cut -d' ' -f1 | grep -v "^$V1_ID$" | head -1)
test -n "$NEW_ID"
"$AUTO" registry get "$NEW_ID" --out "$WORK/v2.cbin" --registry "$REG" > /dev/null
"$AUTO" run --artifact "$WORK/v2.cbin" --input "$FAR" | tee "$WORK/ratchet.out"
grep -q '"qqqqq xxxxx zzzzz"' "$WORK/ratchet.out"

echo "e2e OK: guard tripped, tier-0 answered and ingested, auto daemon --once recompiled through the real gate and published, and the once-novel input now answers tier-1 — the ratchet with no human in the loop (plus: the split-conformal guard flag reached the wire and served tier-1)"
