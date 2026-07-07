#!/usr/bin/env bash
# AUTO-BENCH v1 — F5 summarize-strict: judged differential over the F2
# store (evals/bench/DESIGN.md; the generative residue: judged parity or
# honest refusal).
#
#   bash evals/bench/run-f5.sh OUT_DIR STORE [SESSION] [CAP]
#
# Run from the REPO ROOT. STORE is the F2 store (run-f2.sh recording —
# F5 makes NO recording of its own; the residue is the wave-5 lesson's
# 20%-deterministic summarize span). SESSION default bench-f5, CAP = USD
# spend cap for the JUDGE (ADR-0010 rails; default 0 = fail-closed:
# every judge call refused).
#
# Three sub-runs against evals/bench/bench-f5.contract.toml
# (differential_match = "judged", min agreement 800 milli — committed
# before any run):
#   (a) distill --divergent-pick weighted, NO --judge-model      [$0]
#       expected INCONCLUSIVE: judged examples/differential without a
#       judge are Unchecked by construction (ADR-0019/0021; wave-9 row 1
#       measured exactly this). The blocked emit is the result.
#   (b) same + --judge-model gpt-5.4-mini                        [PAID]
#       the headline: verdict + measured agreement fraction. Wave-12
#       measured expectation on this store shape: PASS at 17/20 = 85%
#       (three feature-collision leaves JUDGED not equivalent, priced by
#       the declared 800).
#   (c) control: --divergent-pick most-common --holdout 0 + judge [PAID]
#       wave-9 measured expectation: PASS 20/20 with 0 live consults (a
#       memorizing subject reproduces its own canonical picks byte-equal;
#       judge arbitration structurally unreachable — the control shows
#       the judge matters exactly for non-memorizing subjects).
#
# JUDGE=1 gates the paid sub-runs (b) and (c); without it only (a) runs
# and the json says SKIPPED. Judge spend is measured from the ADR-0010
# ledger ($AUTO_SPEND_LEDGER or ~/.auto/spend.jsonl) as a per-sub-run
# session delta and recorded in the json. OPENAI_API_KEY must be in the
# environment or the repo .env for (b)/(c).
#
# FILL GATE: the contract's one example output is a placeholder until
# the orchestrator fills it from THIS store (the script prints the
# recorded outputs + the ADR-0018 canonical pick, then refuses).
#
# Env knobs: AUTO, PY, GUARD_ALPHA_MILLI (default 1), RUNS_DIR,
# BUCKETS (optional tree_train --buckets override; default = trainer's
# 1024 — the wave-9/12 20-ticket store shape passed at default; the
# 60-class wave-5 store needed 4096).
set -euo pipefail

OUT_DIR="${1:?usage: run-f5.sh OUT_DIR STORE [SESSION] [CAP]}"
STORE="${2:?usage: run-f5.sh OUT_DIR STORE [SESSION] [CAP]}"
SESSION="${3:-bench-f5}"
CAP="${4:-0}"

AUTO="${AUTO:-target/debug/auto}"
PY="${PY:-python}"
GUARD_ALPHA_MILLI="${GUARD_ALPHA_MILLI:-1}"
RUNS_DIR="${RUNS_DIR:-evals/runs}"
BUCKETS="${BUCKETS:-}"

CONTRACT=evals/bench/bench-f5.contract.toml
SCRATCH=target/bench/f5
mkdir -p "$OUT_DIR" "$SCRATCH"

if [ ! -f "$STORE" ]; then
  echo "[f5] error: store $STORE does not exist (record it with run-f2.sh RECORD=1 first)" >&2
  exit 2
fi

# ------------------------------------------------------------- fill gate
if grep -q "FROM-RECORDED-REALITY" "$CONTRACT"; then
  "$PY" - "$STORE" <<'PYEOF'
import json, sqlite3, sys
store = sys.argv[1]
example = "I was charged twice for my subscription this month, please refund the duplicate payment."
con = sqlite3.connect(store)
rows = con.execute(
    "SELECT s.output, COUNT(*) FROM spans s JOIN traces t ON t.trace_id = s.trace_id "
    "WHERE t.task = 'inbox-agent' AND s.kind = 'model_call' AND s.name = 'summarize' "
    "AND s.error IS NULL AND s.input = ? GROUP BY s.output",
    (json.dumps({"ticket": example}, sort_keys=True, separators=(",", ":")),),
).fetchall()
print("[f5] FILL GATE: evals/bench/bench-f5.contract.toml still has <FROM-RECORDED-REALITY>.")
print("[f5] recorded summarize outputs for the frozen example ticket (corpus line 1):")
if not rows:
    print("    (no recorded observations — run run-f2.sh with RECORD=1 first)")
else:
    for out, n in rows:
        print(f"    recorded {n}x: {out}")
    # ADR-0018 canonical pick: majority witness; ties -> lexicographically
    # smaller canonical (JSON-encoded) output. The stored output IS the
    # canonical JSON text, so sorting the stored strings matches the gate.
    pick = sorted(rows, key=lambda r: (-r[1], r[0]))[0][0]
    print(f"    canonical pick (fill this, WITHOUT the JSON quotes): {pick}")
PYEOF
  echo "[f5] fill the example output (recorded reality), commit the fill, rerun." >&2
  exit 2
fi

# ------------------------------------------------ three gated sub-runs
export F5_OUT_DIR="$OUT_DIR" F5_STORE="$STORE" F5_SESSION="$SESSION" F5_CAP="$CAP"
export F5_AUTO="$AUTO" F5_CONTRACT="$CONTRACT" F5_ALPHA="$GUARD_ALPHA_MILLI"
export F5_RUNS_DIR="$RUNS_DIR" F5_JUDGE="${JUDGE:-0}" F5_BUCKETS="$BUCKETS"
"$PY" - <<'PYEOF'
import json, os, re, subprocess, time

OUT = os.environ["F5_OUT_DIR"]
STORE = os.environ["F5_STORE"]
# CreateProcess rejects forward-slash RELATIVE program paths (measured);
# bare names resolve via PATH — normalize only path-bearing values.
AUTO = os.environ["F5_AUTO"]
if os.path.dirname(AUTO):
    AUTO = os.path.abspath(AUTO)
CONTRACT = os.environ["F5_CONTRACT"]
ALPHA = os.environ["F5_ALPHA"]
RUNS_DIR = os.environ["F5_RUNS_DIR"]
SESSION = os.environ["F5_SESSION"]
CAP = os.environ["F5_CAP"]
JUDGE_ON = os.environ["F5_JUDGE"] == "1"
BUCKETS = os.environ["F5_BUCKETS"].strip()

TRAINER = "python crates/auto-passes/trainer/tree_train.py"
if BUCKETS:
    TRAINER += f" --buckets {BUCKETS}"

LEDGER = os.environ.get("AUTO_SPEND_LEDGER", "").strip() or os.path.join(
    os.path.expanduser("~"), ".auto", "spend.jsonl")


def session_spend():
    """Sum ledgered µ$ for SESSION (ADR-0010 ledger; absent file = 0)."""
    total, purposes = 0, {}
    try:
        with open(LEDGER, encoding="utf-8") as f:
            for line in f:
                if not line.strip():
                    continue
                e = json.loads(line)
                if e.get("session") == SESSION:
                    total += int(e.get("cost_usd_micros", 0))
                    purposes[e.get("purpose", "?")] = purposes.get(e.get("purpose", "?"), 0) + 1
    except OSError:
        pass
    return total, purposes


def sh(argv, log_name):
    p = subprocess.run(argv, capture_output=True, text=True, encoding="utf-8", errors="replace")
    with open(os.path.join(OUT, log_name), "w", encoding="utf-8") as f:
        f.write("$ " + " ".join(argv) + "\n--- stdout ---\n" + p.stdout + "\n--- stderr ---\n" + p.stderr)
    return p.returncode, p.stdout, p.stderr


def parse_sub_run(code, out, err):
    r = {"exit": code}
    m = re.search(r"^verdict: (PASS|FAIL|INCONCLUSIVE)$", out, re.M)
    r["verdict"] = m.group(1) if m else None
    m = re.search(r"differential agreement >= (\d+)/1000.*?measured (\d+)/(\d+) = ([\d.]+)%", out, re.S)
    r["agreement"] = ({"declared_milli": int(m.group(1)), "matched": int(m.group(2)),
                       "eligible": int(m.group(3)), "pct_truncated": float(m.group(4))}
                      if m else None)
    if r["agreement"] is None and re.search(r"differential agreement >= \d+/1000", out):
        r["agreement"] = "unchecked (no judge; judged differential Unchecked by construction)"
    m = re.search(r"weighted witnesses: (\d+) training row\(s\) over (\d+) input\(s\), (\d+) divergent", out)
    if m:
        r["weighted"] = {"rows": int(m.group(1)), "inputs": int(m.group(2)),
                         "divergent_references": int(m.group(3))}
    m = re.search(r"canonical pick: (\d+) trainable input\(s\), (\d+) divergent", out)
    if m:
        r["canonical_pick"] = {"inputs": int(m.group(1)), "divergent_resolved": int(m.group(2))}
    m = re.search(r"train_accuracy ([\d.]+) over (\d+), holdout_accuracy ([\d.]+) over (\d+)", out)
    if m:
        r["train"] = {"train_accuracy": float(m.group(1)), "train_n": int(m.group(2)),
                      "holdout_accuracy": float(m.group(3)), "holdout_n": int(m.group(4))}
    r["judged_equivalent"] = len(re.findall(r"JUDGED equivalent", out))
    r["judged_not_equivalent"] = len(re.findall(r"JUDGED not\s+equivalent", out))
    r["judge_failures"] = len(re.findall(r"judge failed", out))
    m = re.search(r"^eval run ([0-9a-f]+) -> ", out, re.M)
    r["eval_run_id"] = m.group(1) if m else None
    m = re.search(r"^artifact ([0-9a-f]+) -> (.+)$", out, re.M)
    r["artifact_id"] = m.group(1) if m else None
    r["artifact_path"] = m.group(2).strip() if m else None
    if code != 0:
        tail = (err.strip() or out.strip()).splitlines()
        r["refusal_or_block_verbatim"] = "\n".join(tail[-6:]) if tail else "(no output)"
    else:
        r["refusal_or_block_verbatim"] = None
    return r


def distill_cmd(pick, out_art, judged):
    cmd = [AUTO, "distill", "--contract", CONTRACT, "--store", STORE,
           "--trainer", TRAINER, "--model-kind", "tree",
           "--input-field", "ticket", "--holdout", "0", "--seed", "0",
           "--divergent-pick", pick, "--guard-alpha-milli", ALPHA,
           "--out", out_art, "--runs-dir", RUNS_DIR]
    if judged:
        cmd += ["--judge-model", "gpt-5.4-mini", "--spend-cap-usd", CAP, "--session", SESSION]
    return cmd


results = {
    "family": "f5",
    "task": "inbox-agent",
    "span": "summarize",
    "generated_at_unix": int(time.time()),
    "store": STORE,
    "contract": CONTRACT,
    "session": SESSION,
    "cap_usd": CAP,
    "guard_alpha_milli": int(ALPHA),
    "ledger": LEDGER,
    "pins": {"judge_model": "gpt-5.4-mini", "distill_seed": 0, "trainer": TRAINER,
             "declared": "differential_match=judged, min agreement 800 milli (committed pre-run)"},
    "measured_expectations": {
        "a": "INCONCLUSIVE, $0 (wave-9 row 1: judged w/o judge is Unchecked)",
        "b": "verdict + agreement fraction; wave-12 measured PASS 17/20 = 85% on this store shape",
        "c": "wave-9 measured PASS 20/20, 0 live consults (memorizing control)",
    },
}
try:
    results["repo_commit"] = subprocess.run(
        ["git", "rev-parse", "HEAD"], capture_output=True, text=True
    ).stdout.strip() or None
except OSError:
    results["repo_commit"] = None

sub = {}

# (a) weighted, judged contract, NO judge — free, always runs
code, out, err = sh(distill_cmd("weighted", os.path.join(OUT, "f5-weighted-nojudge.cbin"), False),
                    "f5-a-weighted-nojudge.log")
sub["a_weighted_nojudge"] = parse_sub_run(code, out, err)
sub["a_weighted_nojudge"]["judge_model"] = None

# (b) weighted + judge, (c) most-common control + judge — paid, JUDGE=1
for key, pick, art, log in (
        ("b_weighted_judged", "weighted", "f5-weighted.cbin", "f5-b-weighted-judged.log"),
        ("c_mostcommon_judged", "most-common", "f5-mostcommon.cbin", "f5-c-mostcommon-judged.log")):
    if not JUDGE_ON:
        sub[key] = {"status": "SKIPPED (JUDGE!=1; paid judge leg is orchestrator-fired)"}
        continue
    before, _ = session_spend()
    code, out, err = sh(distill_cmd(pick, os.path.join(OUT, art), True), log)
    sub[key] = parse_sub_run(code, out, err)
    after, purposes = session_spend()
    sub[key]["judge_model"] = "gpt-5.4-mini"
    sub[key]["judge_spend_usd_micros_session_delta"] = after - before
    sub[key]["session_ledger_purposes_seen"] = purposes

results["sub_runs"] = sub
total, _ = session_spend()
results["session_spend_usd_micros_total"] = total
results["status"] = "complete" if JUDGE_ON else "partial (judge legs skipped; rerun with JUDGE=1)"
results["deviations"] = []

out_path = os.path.join(OUT, "f5-results.json")
with open(out_path, "w", encoding="utf-8") as f:
    json.dump(results, f, indent=2, sort_keys=False)
print(f"[f5] results written: {out_path}")
PYEOF
echo "[f5] done."
