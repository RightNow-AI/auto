#!/usr/bin/env bash
# AUTO-BENCH v1 — F2 inbox-agent bench leg (evals/bench/DESIGN.md).
#
#   bash evals/bench/run-f2.sh OUT_DIR STORE [SESSION] [CAP]
#
# Run from the REPO ROOT. OUT_DIR = results dir, STORE = trace store,
# SESSION = ledger session (default bench-f2), CAP = USD cap for
# auto-mediated paid legs (default 0, fail-closed; this script as
# written makes none — recording pays through the agent's own key).
#
# RECORD=1 gates the ONLY paid section: 2 passes x first F2_COUNT
# (default 20) inbox corpus tickets through evals/inbox-agent/agent.py —
# THREE real gpt-5.4-mini calls per run (classify, priority, summarize)
# plus one free local tool (lookup), exactly the wave-5 flow:
# `auto record` sets AUTO_TRACE_FILE for the child, the agent reads
# OPENAI_API_KEY from the environment or the repo .env, AGENT_MODE
# defaults to frontier. 2 x 20 x 3 = 120 paid calls (~190 µ$/run).
#
# DEVIATION (logged, per DESIGN.md): the track spec fixes 20 tickets;
# DESIGN.md's corpus rule says >=40 distinct inputs and pre-registered
# F2 at 40 runs x 2 passes. F2_COUNT=40 restores the pre-registration;
# the default follows the track spec. Either count is recorded in the
# results json — never silently absorbed. Note the wave-9/12 judged
# firings ran on this exact 20-ticket shape, so F5's measured
# expectations assume F2_COUNT=20.
#
# Offline: census (canonical report + per-span-name determinism computed
# from the store, mirroring determinism.rs rules) -> enum-synth compiles
# of the classify + priority spans against their EXISTING committed
# contracts (evals/inbox-agent/*.contract.toml, verbatim — anti-gaming:
# contracts are never tuned for the bench; see the expectation notes in
# the results json) -> H4 probes (probes-f2.jsonl) against whatever
# emitted -> f2-results.json. The summarize residue is F5's job
# (run-f5.sh over THIS store).
#
# Env knobs: AUTO, PY, GUARD_ALPHA_MILLI (default 1), RUNS_DIR, F2_COUNT.
set -euo pipefail

OUT_DIR="${1:?usage: run-f2.sh OUT_DIR STORE [SESSION] [CAP]}"
STORE="${2:?usage: run-f2.sh OUT_DIR STORE [SESSION] [CAP]}"
SESSION="${3:-bench-f2}"
CAP="${4:-0}"

AUTO="${AUTO:-target/debug/auto}"
PY="${PY:-python}"
GUARD_ALPHA_MILLI="${GUARD_ALPHA_MILLI:-1}"
RUNS_DIR="${RUNS_DIR:-evals/runs}"
F2_COUNT="${F2_COUNT:-20}"

CORPUS=evals/inbox-agent/corpus.txt
AGENT=evals/inbox-agent/agent.py
PROBES=evals/bench/probes-f2.jsonl
SCRATCH=target/bench/f2

mkdir -p "$OUT_DIR" "$SCRATCH"

# ---------------------------------------------------------------- record
# PAID (RECORD=1 only). NOT set -e'd per call: a failed run is an honest
# error trace (or a lost call) and must not abort the pass (F4 convention).
if [ "${RECORD:-0}" = "1" ]; then
  echo "[f2] RECORD=1: recording 2 passes x first $F2_COUNT tickets (3 paid calls each; session $SESSION)"
  head -"$F2_COUNT" "$CORPUS" > "$SCRATCH/first.txt"
  for pass in 1 2; do
    echo "[f2] recording pass $pass/2"
    while IFS= read -r t; do
      t="${t%$'\r'}"   # CRLF-checkout insurance
      [ -n "$t" ] || continue
      "$AUTO" record --store "$STORE" -- "$PY" "$AGENT" "$t" </dev/null || true
    done < "$SCRATCH/first.txt"
  done
fi

if [ ! -f "$STORE" ]; then
  echo "[f2] error: store $STORE does not exist (run with RECORD=1 first)" >&2
  exit 2
fi

# ---------------------------------------------------------------- census
"$AUTO" report --task inbox-agent --store "$STORE" > "$OUT_DIR/f2-census.txt"
echo "[f2] census written: $OUT_DIR/f2-census.txt"

# ------------------------------------------- compile/probes/json (all $0)
export F2_OUT_DIR="$OUT_DIR" F2_STORE="$STORE" F2_SESSION="$SESSION" F2_CAP="$CAP"
export F2_AUTO="$AUTO" F2_PROBES="$PROBES" F2_ALPHA="$GUARD_ALPHA_MILLI"
export F2_RUNS_DIR="$RUNS_DIR" F2_COUNT
"$PY" - <<'PYEOF'
import json, math, os, re, sqlite3, subprocess, time

OUT = os.environ["F2_OUT_DIR"]
STORE = os.environ["F2_STORE"]
# CreateProcess rejects forward-slash RELATIVE program paths (measured);
# bare names resolve via PATH — normalize only path-bearing values.
AUTO = os.environ["F2_AUTO"]
if os.path.dirname(AUTO):
    AUTO = os.path.abspath(AUTO)
PROBES = os.environ["F2_PROBES"]
ALPHA = os.environ["F2_ALPHA"]
RUNS_DIR = os.environ["F2_RUNS_DIR"]


def sh(argv, log_name):
    p = subprocess.run(argv, capture_output=True, text=True, encoding="utf-8", errors="replace")
    with open(os.path.join(OUT, log_name), "w", encoding="utf-8") as f:
        f.write("$ " + " ".join(argv) + "\n--- stdout ---\n" + p.stdout + "\n--- stderr ---\n" + p.stderr)
    return p.returncode, p.stdout, p.stderr


def pctl(sorted_vals, p):
    if not sorted_vals:
        return None
    k = max(0, math.ceil(p / 100.0 * len(sorted_vals)) - 1)
    return sorted_vals[k]


def parse_census(text):
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
    c["method"] = "auto report --task inbox-agent (canonical determinism report, parsed)"
    return c


def span_groups(con):
    """Signature groups mirroring determinism.rs: (kind, name, input_digest)
    across non-partial traces; effectful = kind != structural 'span'."""
    base = ("SELECT s.kind, s.name, s.input_digest, COUNT(*), "
            "COUNT(DISTINCT s.output_digest), "
            "SUM(CASE WHEN s.error IS NOT NULL THEN 1 ELSE 0 END) "
            "FROM spans s JOIN traces t ON t.trace_id = s.trace_id "
            "WHERE t.task = 'inbox-agent' AND s.kind != 'span'{partial} "
            "GROUP BY s.kind, s.name, s.input_digest")
    try:
        return con.execute(base.format(partial=" AND COALESCE(t.partial, 0) = 0")).fetchall()
    except sqlite3.OperationalError:  # pre-v3 store: no partial column
        return con.execute(base.format(partial="")).fetchall()


def per_span_determinism(con):
    """Per span NAME determinism (the canonical report aggregates per task /
    per kind only). Same witnessing rule: deterministic iff observed >=2,
    zero errors, one distinct output; fractions over witnessed groups."""
    spans = {}
    for kind, name, _sig, obs, outs, errs in span_groups(con):
        s = spans.setdefault(f"{kind}/{name}", {"groups": 0, "witnessed": 0,
                                                "deterministic": 0, "divergent": 0,
                                                "unwitnessed": 0})
        s["groups"] += 1
        if obs >= 2:
            s["witnessed"] += 1
            if errs == 0 and outs == 1:
                s["deterministic"] += 1
            else:
                s["divergent"] += 1
        else:
            s["unwitnessed"] += 1
    for s in spans.values():
        s["deterministic_pct_of_witnessed"] = (
            round(100.0 * s["deterministic"] / s["witnessed"], 1) if s["witnessed"] else None)
    return spans


def baseline(con):
    """H3 frontier side, recorded = baseline: per-RUN latency (root span
    kind='span' name='run' duration) and per-run cost (sum of the three
    model_call spans' reserved cost attrs), plus per-span-name breakdown."""
    root_durs = sorted(r[0] for r in con.execute(
        "SELECT s.duration_ms FROM spans s JOIN traces t ON t.trace_id = s.trace_id "
        "WHERE t.task = 'inbox-agent' AND s.kind = 'span' AND s.name = 'run'"))
    per_run_cost, per_span = {}, {}
    for trace_id, name, dur, attrs, err in con.execute(
            "SELECT s.trace_id, s.name, s.duration_ms, s.attrs, s.error "
            "FROM spans s JOIN traces t ON t.trace_id = s.trace_id "
            "WHERE t.task = 'inbox-agent' AND s.kind = 'model_call'"):
        try:
            a = json.loads(attrs) if attrs else {}
        except ValueError:
            a = {}
        cost = int(a["cost_usd_micros"]) if "cost_usd_micros" in a else 0
        per_run_cost[trace_id] = per_run_cost.get(trace_id, 0) + cost
        ps = per_span.setdefault(name, {"calls": 0, "errors": 0, "durs": [], "cost_sum": 0})
        ps["calls"] += 1
        ps["cost_sum"] += cost
        if err is None:
            ps["durs"].append(int(dur))
        else:
            ps["errors"] += 1
    for name, ps in per_span.items():
        d = sorted(ps.pop("durs"))
        ps["latency_ms"] = {"p50": pctl(d, 50), "p95": pctl(d, 95),
                            "mean": (sum(d) / len(d)) if d else None}
        ps["cost_usd_micros_mean"] = (ps["cost_sum"] / ps["calls"]) if ps["calls"] else None
    runs = sorted(per_run_cost.values())
    return {
        "runs": len(root_durs),
        "run_latency_ms": {"p50": pctl(root_durs, 50), "p95": pctl(root_durs, 95),
                           "mean": (sum(root_durs) / len(root_durs)) if root_durs else None},
        "run_cost_usd_micros": {"mean": (sum(runs) / len(runs)) if runs else None,
                                "sum": sum(runs)},
        "per_span": per_span,
        "source": "recorded root-span duration_ms + model_call reserved cost attrs; "
                  "recorded = baseline (DESIGN.md); percentiles nearest-rank, script-side",
    }


def parse_gate(code, out, err):
    r = {"exit": code}
    m = re.search(r"^verdict: (PASS|FAIL|INCONCLUSIVE)$", out, re.M)
    r["verdict"] = m.group(1) if m else None
    m = re.search(r"differential agreement >= (\d+)/1000.*?measured (\d+)/(\d+) = ([\d.]+)%", out, re.S)
    r["agreement"] = ({"declared_milli": int(m.group(1)), "matched": int(m.group(2)),
                       "eligible": int(m.group(3)), "pct_truncated": float(m.group(4))}
                      if m else None)
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


con = sqlite3.connect(STORE)
results = {
    "family": "f2",
    "task": "inbox-agent",
    "generated_at_unix": int(time.time()),
    "store": STORE,
    "session": os.environ["F2_SESSION"],
    "cap_usd": os.environ["F2_CAP"],
    "cap_note": "no auto-mediated paid leg in this script; recording pays via the agent's own key",
    "guard_alpha_milli": int(ALPHA),
    "recorded_ticket_count": int(os.environ["F2_COUNT"]),
    "pins": {
        "model": "gpt-5.4-mini",
        "prices_usd_per_mtok": {"input": 0.75, "output": 4.50},
        "price_source": "crates/auto-frontier/src/prices.rs (mirrored in evals/inbox-agent/agent.py)",
    },
}
try:
    results["repo_commit"] = subprocess.run(
        ["git", "rev-parse", "HEAD"], capture_output=True, text=True
    ).stdout.strip() or None
except OSError:
    results["repo_commit"] = None

with open(os.path.join(OUT, "f2-census.txt"), encoding="utf-8") as f:
    census_text = f.read()
results["census"] = parse_census(census_text)
results["census"]["raw_report"] = census_text
results["census"]["per_span"] = per_span_determinism(con)
results["census"]["per_span_method"] = (
    "script sqlite groupby (kind, name, input_digest) over non-partial traces, mirroring "
    "determinism.rs: deterministic iff witnessed >=2 AND zero errors AND one distinct output; "
    "the canonical report above aggregates per task/kind only")
results["frontier_baseline"] = baseline(con)

# ---- enum-synth compiles: classify + priority (contracts verbatim) ------
# Expectations (from code reading + wave-5 measurements, recorded here so a
# refusal surprises nobody; the measured verdicts below are the results):
#  * both contracts carry one example whose input is OUTSIDE the first-20
#    window (classify: corpus line 21; priority: line 41) and were filled
#    from the 2026-07-04 store — kept verbatim (anti-gaming: no tuning);
#  * classify over 20 billing tickets: if every witnessed output is the
#    same word, enum synthesis can only propose ConstOut — the out-of-window
#    bug example then fails the subject and blocks the emit (verdict FAIL);
#  * priority: mixed labels -> no ConstOut, no branching op in the v0 DSL
#    -> synthesis refusal expected.
compile_res = {}
for span, contract in (("classify", "evals/inbox-agent/classify.contract.toml"),
                       ("priority", "evals/inbox-agent/priority.contract.toml")):
    art = os.path.join(OUT, f"f2-{span}.cbin")
    cmd = [AUTO, "compile", "--contract", contract, "--store", STORE,
           "--synth", "enum", "--guard-field", "ticket",
           "--guard-alpha-milli", ALPHA,
           "--divergent-pick", "most-common", "--out", art, "--runs-dir", RUNS_DIR]
    code, out, err = sh(cmd, f"f2-compile-{span}.log")
    compile_res[span] = parse_gate(code, out, err)
    compile_res[span]["cmd"] = " ".join(cmd)
    compile_res[span]["contract"] = contract
results["compile"] = compile_res

# ---- H4 probes against whatever emitted ---------------------------------
probes = [json.loads(l) for l in open(PROBES, encoding="utf-8") if l.strip()]
ids = [p["id"] for p in probes]
assert len(ids) == len(set(ids)), "probe ids must be unique"
h4 = {"probes_file": PROBES,
      "exit_codes": "0 = answered (tier-1), 3 = abstained (guard trip, no tier-0)",
      "accounting": "false-abstain = heldout abstained; false-proceed = ood answered (the cardinal failure)",
      "artifacts": {}}
for span in ("classify", "priority"):
    art = os.path.join(OUT, f"f2-{span}.cbin")
    if not os.path.isfile(art):
        h4["artifacts"][f"{span}_v1_jaccard"] = {"status": "NOT RUN (artifact not emitted)"}
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
    h4["artifacts"][f"{span}_v1_jaccard"] = {
        "artifact": art, "per_probe": per,
        "in_dist_total": sum(1 for p in probes if p["kind"] == "heldout"),
        "in_dist_abstained": in_abst,
        "ood_total": sum(1 for p in probes if p["kind"] == "ood"),
        "ood_answered": ood_ans, "other_errors": errs}
results["h4_probes"] = h4

results["status"] = "complete"
results["deviations"] = [
    f"recorded {os.environ['F2_COUNT']} distinct tickets (track spec) vs DESIGN.md's >=40 "
    "corpus rule / 40-run pre-registration; F2_COUNT=40 restores it — logged either way",
    "classify/priority contracts used VERBATIM (committed wave 5): each carries one example "
    "input outside the first-20 window, filled from the 2026-07-04 store — expected to "
    "constrain enum emits; the recorded verdicts in compile.* are the results",
]

out_path = os.path.join(OUT, "f2-results.json")
with open(out_path, "w", encoding="utf-8") as f:
    json.dump(results, f, indent=2, sort_keys=False)
print(f"[f2] results written: {out_path}")
PYEOF
echo "[f2] done."
