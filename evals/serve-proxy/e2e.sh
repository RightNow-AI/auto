#!/usr/bin/env bash
# End-to-end proof of the wave-3 production face over REAL loopback sockets:
# `auto serve` (guard-gated tier-1 serving, honest 409 abstention) and
# `auto proxy` (zero-code-change recording against a labeled fake upstream).
# No external network, no key, no paid call anywhere.
set -euo pipefail
cd "$(dirname "$0")/../.."

AUTO="${AUTO_BIN:-target/debug/auto}"
PY="${PYTHON_BIN:-python3}"
WORK="$(mktemp -d)"
SERVE_PORT=17433
PROXY_PORT=17434
UPSTREAM_PORT=17435
PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${PIDS[@]:-}"; do wait "$pid" 2>/dev/null || true; done
  # windows releases the killed processes' sqlite WAL handles a beat later;
  # scratch removal is best-effort and never fails the e2e verdict
  rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT

wait_http() { # url, tries
  for _ in $(seq 1 "$2"); do
    if curl -s -o /dev/null "$1"; then return 0; fi
    sleep 0.25
  done
  echo "ERROR: $1 never came up"; exit 1
}

# --- 1. build a guarded artifact and a registry (the toy-agent flow; the
# near-variant docs C/D give each witness a real neighbor, so the
# leave-one-out threshold is meaningful — disjoint witnesses calibrate to
# 1.0 and admit everything, spec/runtime.md §2) ------------------------------
STORE="$WORK/store.db"
DOC_B="Compilers translate agent cognition into fast deterministic binaries."
DOC_C="The quick brown fox naps over the lazy dog near the riverbank."
DOC_D="Compilers translate agent cognition into fast deterministic artifacts."
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py "$DOC_B" > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py "$DOC_B" > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py "$DOC_C" > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py "$DOC_D" > /dev/null
"$AUTO" compile --contract evals/toy-agent/fake-frontier.contract.toml --store "$STORE" \
  --guard-field prompt --out "$WORK/guarded.cbin" --runs-dir "$WORK/runs" > /dev/null
"$AUTO" registry keygen --registry "$WORK/reg" > /dev/null
ID=$("$AUTO" registry add "$WORK/guarded.cbin" --sign --registry "$WORK/reg" | sed -n 's/^artifact \([0-9a-f]*\) added.*/\1/p')
test -n "$ID"

# --- 2. auto serve: health, listing, tier-1 answer, honest abstention ------
"$AUTO" serve --registry "$WORK/reg" --addr "127.0.0.1:$SERVE_PORT" 2> "$WORK/serve.log" &
PIDS+=($!)
wait_http "http://127.0.0.1:$SERVE_PORT/health" 40

curl -s "http://127.0.0.1:$SERVE_PORT/health" | tee "$WORK/health.json" | grep -q '"ok":true'
curl -s "http://127.0.0.1:$SERVE_PORT/artifacts" | grep -q "$ID"

# in-distribution input answers tier-1 over the socket
curl -s -X POST "http://127.0.0.1:$SERVE_PORT/run/$ID" \
  -d '{"prompt":"The quick brown fox jumps over the lazy dog near the riverbank."}' \
  | tee "$WORK/run-in.json" | grep -q '"brown jumps quick"'

# a far input trips the guard: 409, abstained, never answered by the module
CODE=$(curl -s -o "$WORK/run-far.json" -w "%{http_code}" -X POST \
  "http://127.0.0.1:$SERVE_PORT/run/$ID" -d '{"prompt":"zzzzz qqqqq xxxxx"}')
test "$CODE" = "409"
grep -q '"abstained":true' "$WORK/run-far.json"

# unknown artifact id is 404, not an answer
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST \
  "http://127.0.0.1:$SERVE_PORT/run/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff" -d '{}')
test "$CODE" = "404"

# --- 3. auto proxy: relay + ingestion against the labeled fake upstream ----
"$PY" evals/serve-proxy/fake_upstream.py "$UPSTREAM_PORT" &
PIDS+=($!)
"$AUTO" proxy --upstream "http://127.0.0.1:$UPSTREAM_PORT" --store "$WORK/proxy.db" \
  --addr "127.0.0.1:$PROXY_PORT" --task proxy-e2e 2> "$WORK/proxy.log" &
PIDS+=($!)
sleep 1

# a request WITHOUT auth is refused before forwarding
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST \
  "http://127.0.0.1:$PROXY_PORT/v1/chat/completions" \
  -H "content-type: application/json" \
  -d '{"model":"gpt-5.4-mini","messages":[{"role":"user","content":"hi"}]}')
test "$CODE" = "401"

# streaming is refused (not recordable in v0)
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST \
  "http://127.0.0.1:$PROXY_PORT/v1/chat/completions" \
  -H "authorization: Bearer e2e-dummy" -H "content-type: application/json" \
  -d '{"model":"gpt-5.4-mini","stream":true,"messages":[]}')
test "$CODE" = "400"

# a real exchange: relayed verbatim AND ingested as a trace
curl -s -X POST "http://127.0.0.1:$PROXY_PORT/v1/chat/completions" \
  -H "authorization: Bearer e2e-dummy" -H "content-type: application/json" \
  -d '{"model":"gpt-5.4-mini","messages":[{"role":"user","content":"route this ticket"}]}' \
  | tee "$WORK/proxied.json" | grep -q "fake upstream answer"

"$AUTO" report --task proxy-e2e --store "$WORK/proxy.db" | tee "$WORK/proxy-report.txt" \
  | grep -q "1 trace"

echo "e2e OK: serve answered tier-1 over the socket, abstained 409 on a far input, 404 on unknown ids; proxy refused no-auth (401) and streaming (400), relayed the fake upstream verbatim, and ingested the exchange as a trace"
