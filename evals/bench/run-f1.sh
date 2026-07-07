#!/usr/bin/env bash
# AUTO-BENCH v1 — F1 ticket-triage bench leg (evals/bench/DESIGN.md).
#
#   bash evals/bench/run-f1.sh OUT_DIR STORE [SESSION] [CAP]
#
# Run from the REPO ROOT (relative paths; msys mangles colon-bearing
# absolute paths inside embedded argv). OUT_DIR = results directory
# (f1-results.json + raw logs land there), STORE = trace store,
# SESSION = ledger session name (default bench-f1), CAP = USD cap for
# auto-mediated paid legs (default 0, fail-closed; F1's leg as scripted
# makes NONE — recording pays through the agent's own key and every
# offline rung is $0, so CAP is accepted for interface uniformity and
# recorded in the results json).
#
# RECORD=1 gates the ONLY paid section: 2 passes x first 40 corpus
# tickets through evals/ticket-triage/agent.py (~80 calls x ~55 µ$;
# DESIGN.md pre-registers ~$0.005–0.01). Without RECORD=1 the script is
# offline-only and requires an existing store.
#
# Flow: [record] -> census -> fill-gate (bench-f1.contract.toml examples
# must be filled from THIS store; the script prints the recorded outputs
# and refuses to compile past a placeholder) -> compile rungs -> verify
# -> latency probe -> H4 probes -> f1-results.json.
#
# Compile rungs (each refusal is a result, not noise — F3 convention):
#   enum v1:  compile --synth enum --guard-field ticket
#             --divergent-pick most-common   (the track-spec rung).
#             MEASURED EXPECTATION (from code, not yet a measurement):
#             the v0 DSL is a straight-line pipeline with no
#             input-equality branching (crates/auto-dsl/src/lib.rs;
#             ConstOut only when ALL outputs are identical), so 40
#             distinct tickets -> 3 labels has no fitting program and
#             synthesis refuses honestly — the SAME refusal F6's README
#             documents for this exact span. The refusal is recorded
#             verbatim as the enum rung's H3 result.
#   distill:  the rung that measurably reached 60/60 differential parity
#             on this span (wave 3; evals/ticket-triage/README.md), tree
#             trainer, --holdout 0, most-common pick. Emits the v1-guard
#             artifact, then a second run with --guard-embedding emits
#             the v2 artifact for the H4 v2 row.
# If enum unexpectedly emits, its artifacts are used for the probes and
# the distill fallback is skipped (self-correcting; json says which).
#
# Env knobs: AUTO (binary, default target/debug/auto), PY (python),
# GUARD_ALPHA_MILLI (default 1 = v0-equivalent max quantile; rerun the
# offline phase with 200 into a second OUT_DIR for the H4 "both alphas"
# grid — keep alphas identical across families), RUNS_DIR (evals/runs).
set -euo pipefail

OUT_DIR="${1:?usage: run-f1.sh OUT_DIR STORE [SESSION] [CAP]}"
STORE="${2:?usage: run-f1.sh OUT_DIR STORE [SESSION] [CAP]}"
SESSION="${3:-bench-f1}"
CAP="${4:-0}"

AUTO="${AUTO:-target/debug/auto}"
PY="${PY:-python}"
GUARD_ALPHA_MILLI="${GUARD_ALPHA_MILLI:-1}"
RUNS_DIR="${RUNS_DIR:-evals/runs}"

CORPUS=evals/ticket-triage/corpus.txt
AGENT=evals/ticket-triage/agent.py
CONTRACT=evals/bench/bench-f1.contract.toml
PROBES=evals/bench/probes-f1.jsonl
SCRATCH=target/bench/f1

mkdir -p "$OUT_DIR" "$SCRATCH"

# ---------------------------------------------------------------- record
# PAID (RECORD=1 only): 2 passes x first 40 tickets. NOT set -e'd per
# call: a failed record (transient API error) is an honest error span or
# a lost call — logged by `auto record` itself; one must not abort a pass
# (F4 runbook convention). stdin is nulled so no child can eat the corpus.
if [ "${RECORD:-0}" = "1" ]; then
  echo "[f1] RECORD=1: recording 2 passes x first 40 tickets (paid; session $SESSION)"
  head -40 "$CORPUS" > "$SCRATCH/first40.txt"
  for pass in 1 2; do
    echo "[f1] recording pass $pass/2"
    while IFS= read -r t; do
      t="${t%$'\r'}"   # CRLF-checkout insurance
      [ -n "$t" ] || continue
      "$AUTO" record --store "$STORE" -- "$PY" "$AGENT" "$t" </dev/null || true
    done < "$SCRATCH/first40.txt"
  done
fi

if [ ! -f "$STORE" ]; then
  echo "[f1] error: store $STORE does not exist (run with RECORD=1 first)" >&2
  exit 2
fi

# ---------------------------------------------------------------- census
# H2: the canonical determinism report, kept raw AND parsed into the json.
"$AUTO" report --task ticket-triage --store "$STORE" > "$OUT_DIR/f1-census.txt"
echo "[f1] census written: $OUT_DIR/f1-census.txt"

# ------------------------------------------------------------- fill gate
# bench-f1.contract.toml examples are placeholders until the orchestrator
# fills them FROM THIS STORE (and commits the fill). Print the recorded
# outputs to make the fill trivial; never edit the contract from here.
if grep -q "FROM-RECORDED-REALITY" "$CONTRACT"; then
  "$PY" - "$STORE" <<'PYEOF'
import json, sqlite3, sys
store = sys.argv[1]
examples = [
    "I was charged twice for my subscription this month, please refund the duplicate payment.",
    "The app crashes immediately when I open the settings page on Android.",
]
con = sqlite3.connect(store)
print("[f1] FILL GATE: evals/bench/bench-f1.contract.toml still has <FROM-RECORDED-REALITY>.")
print("[f1] recorded outputs for the two frozen example inputs in THIS store:")
for text in examples:
    hits = []
    for out, n in con.execute(
        "SELECT s.output, COUNT(*) FROM spans s JOIN traces t ON t.trace_id = s.trace_id "
        "WHERE t.task = 'ticket-triage' AND s.kind = 'model_call' AND s.name = 'triage' "
        "AND s.error IS NULL AND s.input = ? GROUP BY s.output ORDER BY COUNT(*) DESC, s.output",
        (json.dumps({"ticket": text}, sort_keys=True, separators=(",", ":")),),
    ).fetchall():
        hits.append((out, n))
    print(f"  input: {text[:60]}...")
    if not hits:
        print("    (no recorded observations — record first)")
    for out, n in hits:
        print(f"    recorded {n}x: {out}   (fill WITHOUT the JSON quotes)")
PYEOF
  echo "[f1] fill the two outputs (recorded reality), commit the fill, rerun offline (no RECORD)."
  if [ "${RECORD:-0}" = "1" ]; then
    # the recording phase did its job; the fill is the next human step
    exit 0
  fi
  exit 2
fi

# ------------------------------------------- compile/verify/probes/json
# Everything offline from here, $0: orchestrated by one python program so
# the parsing (gate verdicts, agreement, census) lives in one place.
export F1_OUT_DIR="$OUT_DIR" F1_STORE="$STORE" F1_SESSION="$SESSION" F1_CAP="$CAP"
export F1_AUTO="$AUTO" F1_CONTRACT="$CONTRACT" F1_PROBES="$PROBES" F1_CORPUS="$CORPUS"
export F1_ALPHA="$GUARD_ALPHA_MILLI" F1_RUNS_DIR="$RUNS_DIR"
"$PY" - <<'PYEOF'
import json, math, os, re, sqlite3, subprocess, time

OUT = os.environ["F1_OUT_DIR"]
STORE = os.environ["F1_STORE"]
# CreateProcess rejects forward-slash RELATIVE program paths (measured on
# this host); bare names resolve via PATH, so only path-bearing values are
# normalized.
AUTO = os.environ["F1_AUTO"]
if os.path.dirname(AUTO):
    AUTO = os.path.abspath(AUTO)
CONTRACT = os.environ["F1_CONTRACT"]
PROBES = os.environ["F1_PROBES"]
CORPUS = os.environ["F1_CORPUS"]
ALPHA = os.environ["F1_ALPHA"]
RUNS_DIR = os.environ["F1_RUNS_DIR"]


def sh(argv, log_name):
    """Run argv, tee stdout+stderr to OUT/log_name, return (code, out, err)."""
    p = subprocess.run(argv, capture_output=True, text=True, encoding="utf-8", errors="replace")
    with open(os.path.join(OUT, log_name), "w", encoding="utf-8") as f:
        f.write("$ " + " ".join(argv) + "\n--- stdout ---\n" + p.stdout + "\n--- stderr ---\n" + p.stderr)
    return p.returncode, p.stdout, p.stderr


def parse_census(text):
    """Parse the canonical `auto report` render (auto-trace determinism.rs)."""
    g = lambda pat: re.search(pat, text)
    c = {}
    m = g(r"effectful spans: (\d+)")
    c["effectful_spans"] = int(m.group(1)) if m else None
    m = g(r"witnessed \(signature observed >=2 across runs\): (\d+) of (\d+) spans")
    c["witnessed"], c["witnessed_of"] = (int(m.group(1)), int(m.group(2))) if m else (None, None)
    m = g(r"deterministic: (\d+) spans \(([\d.]+)% of witnessed\)")
    c["deterministic"] = int(m.group(1)) if m else None
    c["deterministic_pct_of_witnessed"] = float(m.group(2)) if m else None
    m = g(r"divergent:\s+(\d+) spans")
    c["divergent"] = int(m.group(1)) if m else None
    c["method"] = "auto report --task ticket-triage (canonical determinism report, parsed)"
    return c


def pctl(sorted_vals, p):
    """Nearest-rank percentile (script-side; labeled as such)."""
    if not sorted_vals:
        return None
    k = max(0, math.ceil(p / 100.0 * len(sorted_vals)) - 1)
    return sorted_vals[k]


def baseline(store):
    """H3 frontier side: recorded = baseline (DESIGN.md). Reads the triage
    spans' real API duration_ms + reserved cost/token attrs (spec/trace.md §3)."""
    con = sqlite3.connect(store)
    rows = con.execute(
        "SELECT s.duration_ms, s.attrs, s.error FROM spans s JOIN traces t ON t.trace_id = s.trace_id "
        "WHERE t.task = 'ticket-triage' AND s.kind = 'model_call' AND s.name = 'triage'"
    ).fetchall()
    durs, costs, toks, errors = [], [], [], 0
    for dur, attrs, err in rows:
        if err is not None:
            errors += 1
        try:
            a = json.loads(attrs) if attrs else {}
        except ValueError:
            a = {}
        if "cost_usd_micros" in a:
            costs.append(int(a["cost_usd_micros"]))
        if "tokens" in a:
            toks.append(int(a["tokens"]))
        if err is None:
            durs.append(int(dur))
    durs.sort()
    return {
        "calls": len(rows),
        "errored_calls": errors,
        "latency_ms": {"p50": pctl(durs, 50), "p95": pctl(durs, 95),
                       "mean": (sum(durs) / len(durs)) if durs else None},
        "cost_usd_micros": {"mean": (sum(costs) / len(costs)) if costs else None,
                            "sum": sum(costs), "calls_with_attrs": len(costs)},
        "tokens": {"mean": (sum(toks) / len(toks)) if toks else None, "sum": sum(toks)},
        "source": "recorded span duration_ms + reserved attrs; recorded = baseline (DESIGN.md); "
                  "percentiles nearest-rank, script-side",
    }


def parse_gate(code, out, err):
    """Pull verdict / agreement / ids out of a compile|distill|verify run."""
    r = {"exit": code}
    m = re.search(r"^verdict: (PASS|FAIL|INCONCLUSIVE)$", out, re.M)
    r["verdict"] = m.group(1) if m else None
    m = re.search(r"differential agreement >= (\d+)/1000.*?measured (\d+)/(\d+) = ([\d.]+)%", out, re.S)
    if m:
        r["agreement"] = {"declared_milli": int(m.group(1)), "matched": int(m.group(2)),
                          "eligible": int(m.group(3)), "pct_truncated": float(m.group(4))}
    else:
        r["agreement"] = None
    m = re.search(r"^eval run ([0-9a-f]+) -> ", out, re.M)
    r["eval_run_id"] = m.group(1) if m else None
    m = re.search(r"^artifact ([0-9a-f]+) -> (.+)$", out, re.M)
    r["artifact_id"] = m.group(1) if m else None
    r["artifact_path"] = m.group(2).strip() if m else None
    if code != 0:
        tail = (err.strip() or out.strip()).splitlines()
        r["refusal_verbatim"] = "\n".join(tail[-6:]) if tail else "(no output)"
    else:
        r["refusal_verbatim"] = None
    return r


results = {
    "family": "f1",
    "task": "ticket-triage",
    "generated_at_unix": int(time.time()),
    "store": STORE,
    "session": os.environ["F1_SESSION"],
    "cap_usd": os.environ["F1_CAP"],
    "cap_note": "no auto-mediated paid leg in this script; recording pays via the agent's own key",
    "guard_alpha_milli": int(ALPHA),
    "pins": {
        "model": "gpt-5.4-mini",
        "prices_usd_per_mtok": {"input": 0.75, "output": 4.50},
        "price_source": "crates/auto-frontier/src/prices.rs (mirrored in evals/ticket-triage/agent.py)",
        "distill_seed": 0,
        "trainer": "python crates/auto-passes/trainer/tree_train.py (tree)",
    },
}
try:
    results["repo_commit"] = subprocess.run(
        ["git", "rev-parse", "HEAD"], capture_output=True, text=True
    ).stdout.strip() or None
except OSError:
    results["repo_commit"] = None

with open(os.path.join(OUT, "f1-census.txt"), encoding="utf-8") as f:
    census_text = f.read()
results["census"] = parse_census(census_text)
results["census"]["raw_report"] = census_text
results["frontier_baseline"] = baseline(STORE)

# ---- compile rungs ------------------------------------------------------
compile_res = {}
v1_art = os.path.join(OUT, "f1-triage-v1.cbin")
v2_art = os.path.join(OUT, "f1-triage-v2.cbin")

enum_cmd = [AUTO, "compile", "--contract", CONTRACT, "--store", STORE,
            "--synth", "enum", "--guard-field", "ticket",
            "--guard-alpha-milli", ALPHA,
            "--divergent-pick", "most-common", "--out", v1_art, "--runs-dir", RUNS_DIR]
code, out, err = sh(enum_cmd, "f1-compile-enum-v1.log")
compile_res["enum_v1"] = parse_gate(code, out, err)
compile_res["enum_v1"]["cmd"] = " ".join(enum_cmd)

enum_emitted = compile_res["enum_v1"]["artifact_id"] is not None
if enum_emitted:
    cmd = enum_cmd[:-4] + ["--guard-embedding", "--out", v2_art, "--runs-dir", RUNS_DIR]
    code, out, err = sh(cmd, "f1-compile-enum-v2.log")
    compile_res["enum_v2"] = parse_gate(code, out, err)
    compile_res["enum_v2"]["cmd"] = " ".join(cmd)
    results["artifact_rung"] = "enum (synthesis unexpectedly fit a program; distill skipped)"
else:
    base = [AUTO, "distill", "--contract", CONTRACT, "--store", STORE,
            "--trainer", "python crates/auto-passes/trainer/tree_train.py",
            "--model-kind", "tree", "--input-field", "ticket",
            "--holdout", "0", "--seed", "0",
            "--divergent-pick", "most-common",
            "--guard-alpha-milli", ALPHA, "--runs-dir", RUNS_DIR]
    cmd = base + ["--out", v1_art]
    code, out, err = sh(cmd, "f1-distill-v1.log")
    compile_res["distill_v1"] = parse_gate(code, out, err)
    compile_res["distill_v1"]["cmd"] = " ".join(cmd)
    cmd = base + ["--guard-embedding", "--out", v2_art]
    code, out, err = sh(cmd, "f1-distill-v2.log")
    compile_res["distill_v2"] = parse_gate(code, out, err)
    compile_res["distill_v2"]["cmd"] = " ".join(cmd)
    results["artifact_rung"] = "distill (enum refused, as the DSL predicts; refusal recorded verbatim)"
results["compile"] = compile_res

# ---- verify (trace mode, content-addressed eval run) --------------------
code, out, err = sh([AUTO, "verify", "--contract", CONTRACT, "--store", STORE,
                     "--runs-dir", RUNS_DIR], "f1-verify.log")
results["verify"] = parse_gate(code, out, err)

# ---- latency probe: 20 warm one-shot runs on one witnessed input --------
with open(CORPUS, encoding="utf-8") as f:
    witnessed = f.readline().strip()
probe_input = json.dumps({"ticket": witnessed})
lat = {"artifact": v1_art if os.path.isfile(v1_art) else None,
       "input_ticket": witnessed, "timer": "script-timer",
       "note": "one-shot `auto run` process wall-clock incl. spawn + wasm compile "
               "(deployment-shaped, F6 convention); 1 unmeasured warmup then 20 measured; "
               "resident/in-process floors measured elsewhere (paper/log.md waves 6/9/10)"}
if lat["artifact"]:
    argv = [AUTO, "run", "--artifact", v1_art, "--input", probe_input]
    subprocess.run(argv, capture_output=True)  # warmup, unmeasured
    times, outputs = [], set()
    for _ in range(20):
        t0 = time.perf_counter()
        p = subprocess.run(argv, capture_output=True, text=True, encoding="utf-8", errors="replace")
        times.append((time.perf_counter() - t0) * 1000.0)
        outputs.add((p.returncode, p.stdout.strip()))
    s = sorted(times)
    lat.update(calls=20, ms=[round(t, 2) for t in times],
               p50_ms=round(pctl(s, 50), 2), p95_ms=round(pctl(s, 95), 2),
               mean_ms=round(sum(s) / len(s), 2), min_ms=round(s[0], 2), max_ms=round(s[-1], 2),
               distinct_outcomes=sorted(f"exit={c} out={o}" for c, o in outputs))
else:
    lat["note"] = "NOT RUN: no artifact emitted (every rung refused); " + lat["note"]
results["latency_probe"] = lat

# ---- H4 probes: per artifact (v1 + v2 wires), exit 0/3 accounting -------
probes = [json.loads(l) for l in open(PROBES, encoding="utf-8") if l.strip()]
ids = [p["id"] for p in probes]
assert len(ids) == len(set(ids)), "probe ids must be unique"
h4 = {"probes_file": PROBES, "exit_codes": "0 = answered (tier-1), 3 = abstained (guard trip, no tier-0)",
      "accounting": "false-abstain = heldout abstained; false-proceed = ood answered (the cardinal failure)",
      "artifacts": {}}
for wire, art in (("v1_jaccard", v1_art), ("v2_embedding", v2_art)):
    if not os.path.isfile(art):
        h4["artifacts"][wire] = {"status": "NOT RUN (artifact not emitted)"}
        continue
    per, in_abst, ood_ans, errs = [], 0, 0, 0
    for pr in probes:
        p = subprocess.run([AUTO, "run", "--artifact", art, "--input",
                            json.dumps({"ticket": pr["ticket"]})],
                           capture_output=True, text=True, encoding="utf-8", errors="replace")
        rec = {"id": pr["id"], "kind": pr["kind"], "exit": p.returncode}
        if p.returncode == 0:
            rec["answered"] = p.stdout.strip()
            if pr["kind"] == "ood":
                ood_ans += 1
        elif p.returncode == 3:
            if pr["kind"] == "heldout":
                in_abst += 1
        else:
            errs += 1
            rec["error"] = (p.stderr.strip().splitlines() or ["?"])[-1]
        per.append(rec)
    n_h = sum(1 for p in probes if p["kind"] == "heldout")
    n_o = sum(1 for p in probes if p["kind"] == "ood")
    h4["artifacts"][wire] = {"artifact": art, "per_probe": per,
                             "in_dist_total": n_h, "in_dist_abstained": in_abst,
                             "ood_total": n_o, "ood_answered": ood_ans, "other_errors": errs}
results["h4_probes"] = h4

results["status"] = "complete"
results["deviations"] = [
    "enum rung expected to refuse (v0 DSL has no input-equality branching; F6 README) — "
    "artifacts for latency/H4 come from the distill rung unless enum emitted; see compile.*",
    "bench-f1.contract.toml used instead of evals/ticket-triage/triage.contract.toml: the "
    "wave-3 contract's feature example input lies OUTSIDE the first-40 bench window and its "
    "examples were filled from a different (2026-07-04) store; see the contract header",
]

out_path = os.path.join(OUT, "f1-results.json")
with open(out_path, "w", encoding="utf-8") as f:
    json.dump(results, f, indent=2, sort_keys=False)
print(f"[f1] results written: {out_path}")
PYEOF
echo "[f1] done."
