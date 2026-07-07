#!/usr/bin/env bash
# End-to-end proof of the S1 pipeline: record the toy agent twice through the
# real python SDK, then check the determinism report's measured numbers.
# Expected: 10 effectful spans, 8 deterministic (4 signatures x 2 runs),
# 2 divergent (the wall-clock tool x 2). No network anywhere.
set -euo pipefail
cd "$(dirname "$0")/../.."

AUTO="${AUTO_BIN:-target/debug/auto}"
PY="${PYTHON_BIN:-python3}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
STORE="$WORK/store.db"

"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py
"$AUTO" report --task toy-agent --store "$STORE" | tee "$WORK/report.txt"

grep -q 'determinism report — task "toy-agent" (2 traces)' "$WORK/report.txt"
grep -q 'effectful spans: 10 (structural spans excluded: 2)' "$WORK/report.txt"
grep -q 'deterministic: 8 spans (80.0% of witnessed)' "$WORK/report.txt"
grep -q 'divergent:     2 spans' "$WORK/report.txt"
grep -q 'clock.now_ms' "$WORK/report.txt"

# task-level determinism (ADR-0025): the agent records task_input +
# set_task_output, so the report gains its task-level section — two runs of
# the same document are one witnessed, deterministic task signature
grep -q 'task-level determinism (traces recording task input+output):' "$WORK/report.txt"
grep -q 'observations: 2 (partial task I/O traces excluded: 0)' "$WORK/report.txt"
grep -q 'deterministic: 2 observations (100.0% of witnessed)' "$WORK/report.txt"

"$AUTO" verify --contract evals/toy-agent/fake-frontier.contract.toml \
  --store "$STORE" --runs-dir "$WORK/runs" | tee "$WORK/verify.txt"
grep -q 'verdict: PASS' "$WORK/verify.txt"
grep -q 'eval run ' "$WORK/verify.txt"
ls "$WORK"/runs/*.json > /dev/null

# task-scope contract, trace mode (ADR-0025): the whole-run I/O verifies
# against the recorded task input/output — the S2 gap closes
"$AUTO" verify --contract evals/toy-agent/task.contract.toml \
  --store "$STORE" --runs-dir "$WORK/runs" | tee "$WORK/task-verify.txt"
grep -q 'task-level observations present (2 of 2 traces record task input+output)' "$WORK/task-verify.txt"
grep -q 'example "riverbank-run" (2 matching observation(s))' "$WORK/task-verify.txt"
grep -q 'verdict: PASS' "$WORK/task-verify.txt"

# --- S3: compile the fake-frontier span into an artifact, gated on PASS ---
IMPL_MANIFEST=evals/toy-agent/fake-frontier-impl/Cargo.toml
IMPL_OUT=evals/toy-agent/fake-frontier-impl/target/wasm32-unknown-unknown/release/fake_frontier_impl.wasm
cargo build --quiet --release --target wasm32-unknown-unknown --manifest-path "$IMPL_MANIFEST"
cp "$IMPL_OUT" "$WORK/right.wasm"
cargo build --quiet --release --target wasm32-unknown-unknown --manifest-path "$IMPL_MANIFEST" --features wrong
cp "$IMPL_OUT" "$WORK/wrong.wasm"

"$AUTO" compile --contract evals/toy-agent/fake-frontier.contract.toml \
  --store "$STORE" --module "$WORK/right.wasm" \
  --out "$WORK/fake-frontier.cbin" --runs-dir "$WORK/runs" | tee "$WORK/compile.txt"
grep -q 'verdict: PASS' "$WORK/compile.txt"
grep -q 'artifact ' "$WORK/compile.txt"

"$AUTO" run --artifact "$WORK/fake-frontier.cbin" \
  --input '{"prompt":"The quick brown fox jumps over the lazy dog near the riverbank."}' \
  | tee "$WORK/run.txt"
grep -q '"brown jumps quick"' "$WORK/run.txt"

"$AUTO" inspect "$WORK/fake-frontier.cbin" | tee "$WORK/inspect.txt"
grep -q 'capabilities: none' "$WORK/inspect.txt"
grep -q 'graph.air' "$WORK/inspect.txt"

# the gate must BLOCK a wrong implementation — no artifact, nonzero exit
if "$AUTO" compile --contract evals/toy-agent/fake-frontier.contract.toml \
  --store "$STORE" --module "$WORK/wrong.wasm" \
  --out "$WORK/wrong.cbin" --runs-dir "$WORK/runs" > "$WORK/wrong-out.txt" 2> "$WORK/wrong-err.txt"; then
  echo "ERROR: wrong module compiled — the emit gate is broken"
  exit 1
fi
grep -q 'emit blocked' "$WORK/wrong-err.txt"
grep -q 'verdict: FAIL' "$WORK/wrong-out.txt"
test ! -f "$WORK/wrong.cbin"

# --- S4: synthesize the implementation (no --module) from a two-document store ---
STORE2="$WORK/store2.db"
DOC_B="Compilers translate agent cognition into fast deterministic binaries."
"$AUTO" record --store "$STORE2" -- "$PY" evals/toy-agent/agent.py
"$AUTO" record --store "$STORE2" -- "$PY" evals/toy-agent/agent.py
"$AUTO" record --store "$STORE2" -- "$PY" evals/toy-agent/agent.py "$DOC_B"
"$AUTO" record --store "$STORE2" -- "$PY" evals/toy-agent/agent.py "$DOC_B"

"$AUTO" compile --contract evals/toy-agent/fake-frontier.contract.toml \
  --store "$STORE2" --out "$WORK/synth.cbin" --runs-dir "$WORK/runs" | tee "$WORK/synth.txt"
grep -q 'synthesized: ' "$WORK/synth.txt"
grep -q 'verdict: PASS' "$WORK/synth.txt"

# HELD-OUT document: the synthesized program must GENERALIZE, not memorize
"$AUTO" run --artifact "$WORK/synth.cbin" \
  --input '{"prompt":"Traces prove most model calls are secretly parsers."}' \
  | tee "$WORK/synth-run.txt"
grep -q '"calls model parsers"' "$WORK/synth-run.txt"

"$AUTO" inspect "$WORK/synth.cbin" | tee "$WORK/synth-inspect.txt"
grep -q 'program.json' "$WORK/synth-inspect.txt"
grep -q 'S4 synthesized compile' "$WORK/synth-inspect.txt"

# --- S6: tiering — guarded compile, calibrated abstention, deopt, the ratchet ---
# Two more documents join STORE2 first: DOC_A and DOC_B share zero trigrams,
# so on their own the leave-one-out threshold calibrates to 1.0 — a guard
# that admits everything (spec/runtime.md). Near-variants give every witness
# a real neighbor, so a far input genuinely trips.
DOC_C="The quick brown fox naps over the lazy dog near the riverbank."
DOC_D="Compilers translate agent cognition into fast deterministic artifacts."
"$AUTO" record --store "$STORE2" -- "$PY" evals/toy-agent/agent.py "$DOC_C"
"$AUTO" record --store "$STORE2" -- "$PY" evals/toy-agent/agent.py "$DOC_D"

"$AUTO" compile --contract evals/toy-agent/fake-frontier.contract.toml \
  --store "$STORE2" --guard-field prompt \
  --out "$WORK/guarded.cbin" --runs-dir "$WORK/runs" | tee "$WORK/guarded.txt"
grep -q 'verdict: PASS' "$WORK/guarded.txt"
grep -q 'guard: 4 witness(es)' "$WORK/guarded.txt"

# in-distribution input: the guard proceeds, tier-1 answers
"$AUTO" run --artifact "$WORK/guarded.cbin" \
  --input '{"prompt":"The quick brown fox jumps over the lazy dog near the riverbank."}' \
  > "$WORK/guarded-run.txt" 2> "$WORK/guarded-run-err.txt"
grep -q '"brown jumps quick"' "$WORK/guarded-run.txt"
grep -q 'guard: proceed' "$WORK/guarded-run-err.txt"

# far input, no tier-0: calibrated abstention — exit 3, no answer invented.
# (This prompt's oracle answer is non-empty on purpose: once deopted and
# ingested it faces the same contract, whose len_range property refuses
# empty outputs.)
FAR_INPUT='{"prompt":"zzzzz qqqqq xxxxx"}'
set +e
"$AUTO" run --artifact "$WORK/guarded.cbin" --input "$FAR_INPUT" \
  > "$WORK/abstain-out.txt" 2> "$WORK/abstain-err.txt"
ABSTAIN_EXIT=$?
set -e
test "$ABSTAIN_EXIT" -eq 3
grep -q 'guard tripped' "$WORK/abstain-err.txt"
grep -q 'refusing' "$WORK/abstain-err.txt"
test ! -s "$WORK/abstain-out.txt"

# same far input, tier-0 configured: deopt — the oracle answers, and the
# observation is ingested into the store as a synthetic trace
"$AUTO" run --artifact "$WORK/guarded.cbin" --input "$FAR_INPUT" \
  --tier0 "$PY evals/toy-agent/tier0_oracle.py" --store "$STORE2" \
  > "$WORK/deopt-out.txt" 2> "$WORK/deopt-err.txt"
grep -q '"qqqqq xxxxx zzzzz"' "$WORK/deopt-out.txt"
grep -q 'guard tripped' "$WORK/deopt-err.txt"
grep -q 'deopt' "$WORK/deopt-err.txt"
grep -q 'ingested' "$WORK/deopt-err.txt"

# recompile: synthesis and the guard now see 5 distinct witnesses (the
# ingested far observation included)
"$AUTO" compile --contract evals/toy-agent/fake-frontier.contract.toml \
  --store "$STORE2" --guard-field prompt \
  --out "$WORK/guarded2.cbin" --runs-dir "$WORK/runs" | tee "$WORK/guarded2.txt"
grep -q 'verdict: PASS' "$WORK/guarded2.txt"
grep -q 'guard: 5 witness(es)' "$WORK/guarded2.txt"

# THE RATCHET: the once-novel input is now a witness (distance 0.0) — tier-1
# answers it with no tier-0 configured. Nothing figured out twice. (v0 quirk,
# stated in spec/runtime.md: the far witness has no neighbor, so leave-one-out
# recalibrates this guard's threshold to 1.0.)
"$AUTO" run --artifact "$WORK/guarded2.cbin" --input "$FAR_INPUT" \
  > "$WORK/ratchet-out.txt" 2> "$WORK/ratchet-err.txt"
grep -q '"qqqqq xxxxx zzzzz"' "$WORK/ratchet-out.txt"
grep -q 'guard: proceed' "$WORK/ratchet-err.txt"

# --- S7: registry — content-addressed store, detached ed25519 signatures ---
REG="$WORK/registry"
"$AUTO" registry keygen --registry "$REG"
"$AUTO" registry add "$WORK/guarded2.cbin" --sign --registry "$REG" | tee "$WORK/reg-add.txt"
grep -q 'signed' "$WORK/reg-add.txt"
ID=$(grep -oE '[0-9a-f]{64}' "$WORK/reg-add.txt" | head -n 1)
# the reported id IS the content address: sha-256 of the container bytes
test "$ID" = "$(sha256sum "$WORK/guarded2.cbin" | cut -d' ' -f1)"

"$AUTO" registry list --registry "$REG" | tee "$WORK/reg-list.txt"
grep -q 'toy-agent' "$WORK/reg-list.txt"
grep -q 'verified' "$WORK/reg-list.txt"

"$AUTO" registry get "$ID" --out "$WORK/from-reg.cbin" --registry "$REG"
"$AUTO" run --artifact "$WORK/from-reg.cbin" \
  --input '{"prompt":"The quick brown fox jumps over the lazy dog near the riverbank."}' \
  | tee "$WORK/from-reg-run.txt"
grep -q '"brown jumps quick"' "$WORK/from-reg-run.txt"

# tamper: flip one byte of the stored artifact — get must refuse. The id is
# recomputed from the bytes served, so corruption cannot be served silently.
"$PY" - "$REG/artifacts/$ID.cbin" <<'PYEOF'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
data = bytearray(path.read_bytes())
data[-1] ^= 0xFF
path.write_bytes(bytes(data))
PYEOF
if "$AUTO" registry get "$ID" --out "$WORK/tampered.cbin" --registry "$REG" \
  > "$WORK/tamper.txt" 2>&1; then
  echo "ERROR: tampered artifact served — content addressing is broken"
  exit 1
fi
grep -q 'id mismatch' "$WORK/tamper.txt"

echo "e2e OK: 80.0% deterministic measured, contract PASS, task-scope contract PASS in trace mode (ADR-0025), hand artifact ran tier-1, wrong impl blocked, synthesis generalized to a held-out doc, guard proceeded in-distribution and tripped far, abstained without tier-0 (exit 3), deopt answer ingested and recompiled to tier-1 (the ratchet), registry signed/verified/served by content id, tampered bytes refused"
