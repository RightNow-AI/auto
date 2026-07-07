#!/usr/bin/env bash
# End-to-end proof of the remote registry transport (ADR-0022) over REAL
# loopback sockets: publish + sign into a local registry, serve a second root
# over HTTP, PUSH to it, PULL into a FRESH root, verify the pulled copy by
# content id AND signature, then tamper the served bytes and prove the pull
# REFUSES. Content addressing is verified at both ends, so tamper evidence
# survives the wire. No external network, no key, no paid call anywhere.
#
# CLI verbs `registry serve|push|pull` are wired by the orchestrator against
# the frozen protocol in spec/registry.md §6; this script drives exactly those.
set -euo pipefail
cd "$(dirname "$0")/../.."

AUTO="${AUTO_BIN:-target/debug/auto}"
PY="${PYTHON_BIN:-python3}"
PORT="${REGISTRY_REMOTE_PORT:-17436}"
BASE="http://127.0.0.1:$PORT"

# RELATIVE work dir under target/: msys mangles colon-bearing absolute path
# args, so every --registry / --out path stays relative (URLs are fine).
WORK="target/registry-remote-e2e"
rm -rf "$WORK"
mkdir -p "$WORK"
LOCAL="$WORK/local"     # publishing registry (owns the keypair)
SERVER="$WORK/server"   # the served root
FRESH="$WORK/fresh"     # first pull destination
FRESH2="$WORK/fresh2"   # pull-after-tamper destination

PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${PIDS[@]:-}"; do wait "$pid" 2>/dev/null || true; done
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

# --- 1. build a valid, signed artifact via the toy-agent synthesis path
# (no wasm build; compile emits only on contract PASS) -----------------------
STORE="$WORK/store.db"
DOC_B="Compilers translate agent cognition into fast deterministic binaries."
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py "$DOC_B" > /dev/null
"$AUTO" record --store "$STORE" -- "$PY" evals/toy-agent/agent.py "$DOC_B" > /dev/null
"$AUTO" compile --contract evals/toy-agent/fake-frontier.contract.toml \
  --store "$STORE" --out "$WORK/task.cbin" --runs-dir "$WORK/runs" > /dev/null

# --- 2. publish + sign into the LOCAL registry ------------------------------
"$AUTO" registry keygen --registry "$LOCAL" > /dev/null
ID=$("$AUTO" registry add "$WORK/task.cbin" --sign --registry "$LOCAL" \
  | grep -oE '[0-9a-f]{64}' | head -n 1)
test -n "$ID"
# the reported id IS the content address: sha-256 of the container bytes
test "$ID" = "$(sha256sum "$WORK/task.cbin" | cut -d' ' -f1)"

# --- 3. the SERVER root trusts the local verifying key: it must check pushed
# signatures against it and expose it at GET /v0/key. A real deployment
# provisions the org key; here we copy the local public key into the root. ---
mkdir -p "$SERVER/artifacts" "$SERVER/keys"
cp "$LOCAL/keys/auto.pub" "$SERVER/keys/auto.pub"

"$AUTO" registry serve --registry "$SERVER" --addr "127.0.0.1:$PORT" 2> "$WORK/serve.log" &
PIDS+=($!)
wait_http "$BASE/v0/artifacts" 40
# a fresh served root lists nothing yet
test -z "$(curl -s "$BASE/v0/artifacts")"

# --- 4. PUSH local -> server: artifact bytes + detached signature -----------
"$AUTO" registry push "$ID" --remote "$BASE" --registry "$LOCAL" | tee "$WORK/push.txt"
# the id is now listed, served, and has a signature the server accepted
curl -s "$BASE/v0/artifacts" | grep -q "$ID"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v0/artifacts/$ID")" = "200"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v0/artifacts/$ID/signature")" = "200"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v0/key")" = "200"

# a malformed id is a hard 400 (never names an artifact)
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v0/artifacts/not-a-content-id")" = "400"

# --- 5. PULL server -> FRESH root, then verify the pulled copy locally ------
"$AUTO" registry pull "$ID" --remote "$BASE" --registry "$FRESH" | tee "$WORK/pull.txt"
# get recomputes the id and verifies the signature on the PULLED copy
"$AUTO" registry get "$ID" --out "$WORK/pulled.cbin" --registry "$FRESH" | tee "$WORK/get.txt"
grep -q 'signature verified' "$WORK/get.txt"
# byte-identical to the artifact we published
cmp "$WORK/task.cbin" "$WORK/pulled.cbin"

# --- 6. tamper the SERVED bytes; the pull MUST refuse (server recomputes the
# id before serving, so corruption is never handed out) ----------------------
"$PY" - "$SERVER/artifacts/$ID.cbin" <<'PYEOF'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
data = bytearray(path.read_bytes())
data[-1] ^= 0xFF
path.write_bytes(bytes(data))
PYEOF
if "$AUTO" registry pull "$ID" --remote "$BASE" --registry "$FRESH2" \
  > "$WORK/pull2.txt" 2>&1; then
  echo "ERROR: pulled a tampered artifact — content addressing broke across the wire"
  exit 1
fi
# nothing was written into the second fresh root
test ! -f "$FRESH2/artifacts/$ID.cbin"

echo "e2e OK: published + signed locally, served a registry root over loopback HTTP, pushed the artifact + signature, pulled into a fresh root and verified it by content id and signature (byte-identical), and refused a pull after the served bytes were tampered"
