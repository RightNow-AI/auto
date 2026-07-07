#!/usr/bin/env python3
"""AUTO-BENCH v1 - F6 novelty-stream driver (H1, the ratchet curve).

Walks the FROZEN 300-item stream (stream.jsonl, shifts at positions 50 /
120 / 200) against a live `auto` toolchain: the system starts uncompiled,
pays tier-0 for novelty, recompiles every K new distinct inputs, and the
per-item marginal cost decays in steps. This driver makes NO paid call of
its own - every paid call happens inside `auto record` (the reused
evals/ticket-triage/agent.py) or inside `auto run --tier0 frontier:<model>`
(spend-capped, ledgered per ADR-0010). The ORCHESTRATOR fires it and owns
the cap.

Per item (ratchet mode):
  no live artifact yet  -> bootstrap: `auto record --store S -- <python>
                           evals/ticket-triage/agent.py "<ticket>"` - a fresh
                           deployment IS a recording phase; cost/latency read
                           from the recorded span's reserved attrs in the
                           sqlite store (spec/trace.md par.3).
  live artifact exists  -> `auto run --artifact A --input {"ticket":...}
                           --tier0 frontier:MODEL --store S --spend-cap-usd C
                           --session SESS`; stderr tells the tier:
                           "guard: proceed"  -> tier1 (marginal cost 0)
                           "guard tripped"   -> tier0-deopt; cost = spend-
                           ledger delta for SESS; the answer is INGESTED into
                           S under the artifact manifest identity (the
                           ratchet).
  every K new distinct inputs since the last compile attempt -> recompile
  subprocess (default: `auto distill` + the tree trainer - see README for
  why `--synth enum` cannot fit this behavior class); PASS swaps the live
  artifact pointer, refusal/INCONCLUSIVE is logged and the stream continues
  (honest; the curve shows it).

Control mode (--control): `auto run` REQUIRES --artifact (crates/auto-cli
main.rs Run { artifact: PathBuf, .. } - there is no artifact-less pure
tier-0 invocation), so the pure-frontier control is ARITHMETIC, not a second
paid pass: each position is priced at the measured tier-0 cost of its
distinct ticket from the ratchet leg's CSV (mean over that ticket's paid
observations); a ticket never paid in the ratchet leg (possible only via a
guard false-proceed on its first appearance) is priced at the mean paid cost
over all tickets and counted in the `estimated` column. Clearly labeled in
the CSV header; no paid logic here.

Dry-run mode (--dry-run): a labeled FAKE. Walks the stream end-to-end with
deterministic fake tier decisions, fake costs, and fake latencies so the
whole pipeline shape (CSV -> events -> summarize) is inspectable with zero
spend and zero binaries. Every output says FAKE.

Run from the repo root; keep --store/--artifact/--csv/... RELATIVE (the
recompile template embeds paths in argv, and Git Bash's msys layer mangles
colon-bearing absolute paths).
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import re
import shlex
import shutil
import sqlite3
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_STREAM = os.path.join(HERE, "stream.jsonl")
DEFAULT_AGENT = os.path.join("evals", "ticket-triage", "agent.py")
PLACEHOLDER = "<FROM-RECORDED-REALITY>"
SUBPROCESS_TIMEOUT_S = 600

# Default recompile: distill a decision tree (the F1-proven pass; wave-3
# measured 60/60 differential parity on real-LLM triage labels).
# `compile --synth enum` is NOT the default because the enumerative DSL has
# no input-equality branching (crates/auto-dsl/src/lib.rs) - a straight-line
# string pipeline cannot map many distinct tickets to different labels, so
# it refuses honestly on every cycle and the curve never leaves bootstrap.
# Placeholders: {auto} {contract} {store} {out} {runs} {alpha} {python}
DEFAULT_RECOMPILE = (
    '{auto} distill --contract {contract} --store {store} '
    '--trainer "{python} crates/auto-passes/trainer/tree_train.py" '
    '--model-kind tree --input-field ticket --holdout 0 '
    '--divergent-pick most-common --guard-alpha-milli {alpha} '
    '--out {out} --runs-dir {runs}'
)

# stderr markers written by `auto run` (crates/auto-cli/src/main.rs)
RE_PROCEED = re.compile(r"guard: proceed \(distance ([0-9.]+)")
RE_TRIP = re.compile(r"guard tripped: .*\(distance ([0-9.]+|n/a)")
RE_DEOPT_MS = re.compile(r"deopt: tier-0 answered in (\d+)ms")
RE_INGESTED = re.compile(r"deopt: observation ingested as trace (\S+)")
RE_RECORDED = re.compile(r"recorded trace (\S+) task")
RE_ARTIFACT = re.compile(r"^artifact ([0-9a-f]+) -> ", re.MULTILINE)
RE_EVAL_RUN = re.compile(r"^eval run (\S+) -> ", re.MULTILINE)
UNGUARDED = "no guard in artifact"
FATAL_TIER0 = ("spend cap would be exceeded", "no API key")

CSV_FIELDS = [
    "pos",
    "category",
    "text_sha12",
    "distinct_seen",
    "tier",
    "guard_distance",
    "latency_ms",
    "cost_usd_micros",
    "artifact_generation",
    "answer",
]
EVENT_FIELDS = [
    "pos",
    "event",
    "generation",
    "distinct_witnesses",
    "artifact",
    "detail",
]
CONTROL_FIELDS = ["pos", "category", "text_sha12", "cost_usd_micros", "latency_ms", "estimated"]


def sha12(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:12]


def load_stream(path: str) -> list[dict]:
    items = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line:
                items.append(json.loads(line))
    if [it["pos"] for it in items] != list(range(1, len(items) + 1)):
        raise SystemExit(f"error: stream {path} positions are not 1..N in order")
    return items


def run_proc(argv: list[str]) -> tuple[int, str, str, int]:
    """Run argv, return (rc, stdout, stderr, wall_ms). Never raises on rc."""
    started = time.monotonic()
    try:
        proc = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=SUBPROCESS_TIMEOUT_S,
        )
        rc, out, err = proc.returncode, proc.stdout, proc.stderr
    except subprocess.TimeoutExpired as e:
        rc = -1
        out = (e.stdout or b"").decode("utf-8", "replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        err = f"driver: subprocess timed out after {SUBPROCESS_TIMEOUT_S}s"
    except FileNotFoundError as e:
        rc, out, err = -2, "", f"driver: cannot spawn {argv[0]!r}: {e}"
    wall_ms = int((time.monotonic() - started) * 1000)
    return rc, out, err, wall_ms


class Ledger:
    """Session-scoped tail reader over the append-only spend ledger
    (crates/auto-frontier/src/ledger.rs; $AUTO_SPEND_LEDGER or
    ~/.auto/spend.jsonl). The driver never writes it."""

    def __init__(self, path: str, session: str) -> None:
        self.path = path
        self.session = session
        self.offset = self._size()

    def _size(self) -> int:
        try:
            return os.path.getsize(self.path)
        except OSError:
            return 0

    def delta(self) -> tuple[int, int, int]:
        """New (cost_usd_micros, input_tokens+output_tokens, n_lines) for our
        session since the last call. Unparseable tail lines are fatal - a
        wrong total is worse than a halt."""
        size = self._size()
        if size <= self.offset:
            self.offset = size
            return 0, 0, 0
        with open(self.path, "rb") as f:
            f.seek(self.offset)
            blob = f.read()
        self.offset = size
        cost = tokens = n = 0
        for raw in blob.decode("utf-8", "replace").splitlines():
            raw = raw.strip()
            if not raw:
                continue
            entry = json.loads(raw)
            if entry.get("session") != self.session:
                continue
            cost += int(entry["cost_usd_micros"])
            tokens += int(entry.get("input_tokens", 0)) + int(entry.get("output_tokens", 0))
            n += 1
        return cost, tokens, n


def span_cost_from_store(store: str, trace_id: str) -> tuple[int, int]:
    """(cost_usd_micros, duration_ms) of the recorded model_call('triage')
    span of one trace - the reserved attrs agent.py measured from the real
    API usage (spec/trace.md par.3)."""
    conn = sqlite3.connect(store)
    try:
        row = conn.execute(
            "SELECT duration_ms, attrs FROM spans "
            "WHERE trace_id = ? AND kind = 'model_call' AND name = 'triage'",
            (trace_id,),
        ).fetchone()
    finally:
        conn.close()
    if row is None:
        return 0, 0
    duration_ms, attrs_text = row
    try:
        attrs = json.loads(attrs_text)
        cost = int(attrs.get("cost_usd_micros", 0))
    except (ValueError, TypeError):
        cost = 0
    return cost, int(duration_ms)


class CsvSink:
    def __init__(self, path: str, fields: list[str], comments: list[str]) -> None:
        os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
        self.f = open(path, "w", encoding="utf-8", newline="")
        for c in comments:
            self.f.write(f"# {c}\n")
        self.w = csv.DictWriter(self.f, fieldnames=fields)
        self.w.writeheader()

    def row(self, **kw) -> None:
        self.w.writerow(kw)
        self.f.flush()

    def close(self) -> None:
        self.f.close()


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--auto", default=os.path.join("target", "release", "auto.exe"),
                   help="auto binary (relative path preferred)")
    p.add_argument("--store", required=True, help="trace store (fresh file; created by the first record)")
    p.add_argument("--artifact", required=True,
                   help="LIVE artifact pointer path; each gate PASS copies the new generation here")
    p.add_argument("--contract", default=os.path.join("evals", "bench", "f6-stream", "stream.contract.toml"))
    p.add_argument("--session", default="bench-f6", help="spend-ledger session (DESIGN.md per-family name)")
    p.add_argument("--cap", required=True, help="per-session spend cap in USD, passed to every paid subprocess")
    p.add_argument("--csv", required=True, help="per-position CSV out")
    p.add_argument("--events-csv", default=None, help="recompile events CSV (default: <csv>.events.csv)")
    p.add_argument("--stream", default=DEFAULT_STREAM)
    p.add_argument("--model", default="frontier:gpt-5.4-mini", help="--tier0 spec for deopts")
    p.add_argument("--deopt-mode", choices=["frontier", "agent"], default="frontier",
                   help="agent = on guard trip, re-record the item through the REAL agent "
                        "(contract-conformant reference, the honest deployment shape) instead "
                        "of the generic frontier tier-0 interpreter")
    p.add_argument("--recompile-every", type=int, default=8, metavar="K",
                   help="recompile after K new distinct inputs land in the store since the last attempt")
    p.add_argument("--recompile-cmd", default=DEFAULT_RECOMPILE,
                   help="recompile template; placeholders {auto} {contract} {store} {out} {runs} {alpha} {python}")
    p.add_argument("--guard-alpha-milli", type=int, default=100,
                   help="split-conformal alpha (thousandths) handed to the recompile template")
    p.add_argument("--artifacts-dir", default=os.path.join("evals", "bench", "f6-stream", "artifacts"))
    p.add_argument("--runs-dir", default=os.path.join("evals", "runs"))
    p.add_argument("--python", default=sys.executable or "python",
                   help="interpreter for agent.py and the trainer (avoid spaces in the path)")
    p.add_argument("--agent", default=DEFAULT_AGENT)
    p.add_argument("--ledger", default=None,
                   help="spend ledger path (default: $AUTO_SPEND_LEDGER or ~/.auto/spend.jsonl)")
    p.add_argument("--control", action="store_true",
                   help="arithmetic control mode: price every position tier-0 from a ratchet CSV (no paid calls)")
    p.add_argument("--from-csv", default=None, help="(control mode) the ratchet leg's CSV to price from")
    p.add_argument("--dry-run", action="store_true", help="labeled FAKE walk: no binaries, no spend")
    return p.parse_args(argv)


# ---------------------------------------------------------------- control ---

def read_ratchet_csv(path: str) -> list[dict]:
    rows = []
    with open(path, encoding="utf-8") as f:
        body = [line for line in f if not line.startswith("#")]
    for row in csv.DictReader(body):
        rows.append(row)
    return rows


def control_mode(args: argparse.Namespace) -> int:
    if not args.from_csv:
        print("error: --control needs --from-csv <ratchet CSV> (the measured tier-0 prices)", file=sys.stderr)
        return 2
    stream = load_stream(args.stream)
    rows = read_ratchet_csv(args.from_csv)
    paid: dict[str, list[tuple[int, int]]] = {}
    with open(args.from_csv, encoding="utf-8") as f:
        fake_source = any("DRY-RUN" in line for line in f if line.startswith("#"))
    for r in rows:
        cost = int(r["cost_usd_micros"] or 0)
        if r["tier"] in ("bootstrap-record", "tier0-deopt") and cost > 0:
            paid.setdefault(r["text_sha12"], []).append((cost, int(r["latency_ms"] or 0)))
    if not paid:
        print("error: ratchet CSV has no paid tier-0 observations to price the control from", file=sys.stderr)
        return 2
    all_costs = [c for obs in paid.values() for c, _ in obs]
    all_lats = [l for obs in paid.values() for _, l in obs]
    mean_cost = round(sum(all_costs) / len(all_costs))
    mean_lat = round(sum(all_lats) / len(all_lats))
    comments = [
        "AUTO-BENCH v1 F6 - ARITHMETIC CONTROL (counterfactual pure tier-0), no paid calls made for this file.",
        "auto run requires --artifact (no artifact-less tier-0 mode exists), so the control is computed,",
        f"not re-fired: each position priced at the MEAN measured tier-0 cost of its distinct ticket in {args.from_csv};",
        f"tickets never paid there use the global mean paid cost ({mean_cost} u$) and are flagged estimated=1.",
    ]
    if fake_source:
        comments.insert(0, "DRY-RUN SOURCE: the ratchet CSV was a labeled FAKE, so this control is FAKE too.")
    sink = CsvSink(args.csv, CONTROL_FIELDS, comments)
    total = 0
    estimated = 0
    for item in stream:
        digest = sha12(item["ticket"])
        obs = paid.get(digest)
        if obs:
            cost = round(sum(c for c, _ in obs) / len(obs))
            lat = round(sum(l for _, l in obs) / len(obs))
            est = 0
        else:
            cost, lat, est = mean_cost, mean_lat, 1
            estimated += 1
        total += cost
        sink.row(pos=item["pos"], category=item["category"], text_sha12=digest,
                 cost_usd_micros=cost, latency_ms=lat, estimated=est)
    sink.close()
    tag = "FAKE " if fake_source else ""
    print(f"{tag}arithmetic control: {len(stream)} positions, total {total} u$ "
          f"({estimated} positions estimated from the mean) -> {args.csv}")
    return 0


# ---------------------------------------------------------------- dry-run ---

class FakeWorld:
    """Deterministic FAKE tier decisions for --dry-run: guard = 'texts the
    live generation was compiled with proceed, everything else trips';
    pinned fake prices. Labeled FAKE everywhere it surfaces."""

    COST_T0 = 60      # u$ per fake tier-0/bootstrap item
    LAT_T0 = 900      # ms
    LAT_T1 = 45       # ms (one-shot spawn included)

    def __init__(self) -> None:
        self.compiled_texts: set[str] = set()

    def compile_ok(self, witnessed: set[str]) -> None:
        self.compiled_texts = set(witnessed)


def main(argv: list[str]) -> int:
    args = parse_args(argv)

    if args.control:
        return control_mode(args)

    stream = load_stream(args.stream)
    events_path = args.events_csv or (args.csv + ".events.csv")

    if not args.dry_run:
        if not os.path.isfile(args.agent):
            print(f"error: agent {args.agent} not found - run from the repo root", file=sys.stderr)
            return 2
        with open(args.contract, encoding="utf-8") as f:
            if PLACEHOLDER in f.read():
                print(
                    f"error: {args.contract} still contains {PLACEHOLDER}: fill the example "
                    "output from the FIRST recording before a real leg (DESIGN.md: examples "
                    "come from recorded reality). Run --dry-run first if you only need the "
                    "pipeline shape; the first recording itself comes from this driver's "
                    "bootstrap phase run against a store, which needs no example yet - "
                    "record item 1 once via: "
                    f'{args.auto} record --store {args.store} -- {args.python} {args.agent} '
                    '"<pos-1 ticket>"',
                    file=sys.stderr,
                )
                return 2
        if os.path.exists(args.artifact):
            print(f"error: live artifact pointer {args.artifact} already exists; an H1 leg starts "
                  "uncompiled - remove it (and use a fresh store) or point elsewhere", file=sys.stderr)
            return 2
        if os.path.exists(args.store):
            print(f"error: store {args.store} already exists; an H1 leg starts from an empty store "
                  "(a fresh deployment) - remove it or point elsewhere", file=sys.stderr)
            return 2

    ledger_path = args.ledger or os.environ.get("AUTO_SPEND_LEDGER") or os.path.join(
        os.path.expanduser("~"), ".auto", "spend.jsonl")
    ledger = Ledger(ledger_path, args.session)

    mode_banner = (
        "DRY-RUN: every tier decision, cost, and latency below is FAKE (deterministic pinned values; no binary, no spend)"
        if args.dry_run
        else f"REAL leg: session {args.session}, cap {args.cap} USD, tier-0 {args.model}, ledger {ledger_path}"
    )
    print(f"f6-stream driver: {mode_banner}")
    comments = [
        "AUTO-BENCH v1 F6 novelty-stream (H1, the ratchet curve) - per-position log.",
        mode_banner,
        f"stream={args.stream} K={args.recompile_every} guard_alpha_milli={args.guard_alpha_milli}",
        "tier: bootstrap-record = paid recording via evals/ticket-triage/agent.py (no artifact yet);",
        "      tier1 = guard proceed, compiled answer, marginal cost 0; tier0-deopt = guard trip, paid",
        "      frontier answer ingested into the store (the ratchet); abstain/error = honest failures.",
        "cost_usd_micros: bootstrap = reserved span attr from the store; tier0-deopt = spend-ledger",
        "delta for this session; tier1 = 0. latency_ms: bootstrap = recorded span duration (real API",
        "latency); tier0-deopt = the runtime's measured tier-0 wall time; tier1 = one-shot `auto run`",
        "wall time (process spawn + wasm compile included - the honest one-shot number; see README).",
    ]
    if args.dry_run:
        comments.insert(0, "DRY-RUN - FAKE DATA. Not a measurement. Rerun without --dry-run for the real leg.")
    sink = CsvSink(args.csv, CSV_FIELDS, comments)
    events = CsvSink(events_path, EVENT_FIELDS,
                     ["AUTO-BENCH v1 F6 recompile events." + (" DRY-RUN - FAKE DATA." if args.dry_run else "")])

    if not args.dry_run:
        os.makedirs(args.artifacts_dir, exist_ok=True)

    fake = FakeWorld() if args.dry_run else None
    seen_texts: set[str] = set()          # distinct tickets that reached the store
    new_since_attempt = 0                 # distinct tickets since the last compile attempt
    generation = 0
    live_artifact: str | None = None
    totals = {"cost": 0, "paid_calls": 0, "tier1": 0, "deopt": 0, "bootstrap": 0, "error": 0}
    halted = None

    def emit(pos, item, tier, distance, latency_ms, cost, digest, answer=""):
        totals["cost"] += cost
        sink.row(pos=pos, category=item["category"], text_sha12=digest,
                 distinct_seen=len(seen_texts), tier=tier,
                 guard_distance=distance, latency_ms=latency_ms,
                 cost_usd_micros=cost, artifact_generation=generation,
                 answer=answer)

    def recompile(pos: int) -> None:
        nonlocal generation, live_artifact, new_since_attempt
        new_since_attempt = 0
        candidate = os.path.join(args.artifacts_dir, f"f6-gen{generation + 1}.cbin")
        if args.dry_run:
            generation += 1
            fake.compile_ok(seen_texts)
            live_artifact = candidate
            events.row(pos=pos, event="compile-pass(FAKE)", generation=generation,
                       distinct_witnesses=len(seen_texts), artifact=candidate, detail="dry-run fake gate")
            print(f"  [pos {pos}] FAKE recompile -> generation {generation} ({len(seen_texts)} witnesses)")
            return
        # the template is shlex-split POSIX-style, where a bare backslash is
        # an escape and silently eats path separators - normalize every path
        # placeholder to forward slashes (Windows spawns accept them)
        fwd = lambda s: str(s).replace("\\", "/")  # noqa: E731
        cmd = args.recompile_cmd.format(
            auto=fwd(args.auto), contract=fwd(args.contract), store=fwd(args.store),
            out=fwd(candidate), runs=fwd(args.runs_dir), alpha=args.guard_alpha_milli,
            python=fwd(args.python),
        )
        argv = shlex.split(cmd)
        rc, out, err, wall = run_proc(argv)
        m_run = RE_EVAL_RUN.search(out)
        run_id = m_run[1] if m_run else ""
        if rc == 0 and RE_ARTIFACT.search(out):
            generation += 1
            shutil.copyfile(candidate, args.artifact)
            live_artifact = args.artifact
            events.row(pos=pos, event="compile-pass", generation=generation,
                       distinct_witnesses=len(seen_texts), artifact=candidate,
                       detail=f"eval_run={run_id} wall_ms={wall}")
            print(f"  [pos {pos}] recompile PASS -> generation {generation} "
                  f"({len(seen_texts)} witnesses, eval run {run_id})")
        else:
            tail = (err.strip().splitlines() or out.strip().splitlines() or ["no output"])[-1]
            kind = "compile-inconclusive" if "Inconclusive" in (out + err) else "compile-refused"
            events.row(pos=pos, event=kind, generation=generation,
                       distinct_witnesses=len(seen_texts), artifact="",
                       detail=f"rc={rc} eval_run={run_id} {tail[:300]}")
            print(f"  [pos {pos}] recompile {kind.upper()} (rc={rc}): {tail[:160]}")

    for item in stream:
        pos, text = item["pos"], item["ticket"]
        digest = sha12(text)
        is_new = text not in seen_texts

        if args.dry_run:
            if live_artifact is None:
                seen_texts.add(text)
                if is_new:
                    new_since_attempt += 1
                totals["bootstrap"] += 1
                totals["paid_calls"] += 1
                emit(pos, item, "bootstrap-record", "", FakeWorld.LAT_T0, FakeWorld.COST_T0, digest)
            elif text in fake.compiled_texts:
                totals["tier1"] += 1
                emit(pos, item, "tier1", "0.0000", FakeWorld.LAT_T1, 0, digest)
            else:
                seen_texts.add(text)
                if is_new:
                    new_since_attempt += 1
                totals["deopt"] += 1
                totals["paid_calls"] += 1
                emit(pos, item, "tier0-deopt", "0.9000", FakeWorld.LAT_T0, FakeWorld.COST_T0, digest)
            if new_since_attempt >= args.recompile_every:
                recompile(pos)
            continue

        if live_artifact is None:
            # bootstrap: a fresh deployment is a recording phase
            rc, out, err, wall = run_proc(
                [args.auto, "record", "--store", args.store, "--",
                 args.python, args.agent, text])
            if rc != 0:
                tail = (err.strip().splitlines() or ["no stderr"])[-1]
                print(f"driver: bootstrap record failed at pos {pos} (rc={rc}): {tail}", file=sys.stderr)
                halted = f"bootstrap failure at pos {pos}: {tail[:200]}"
                totals["error"] += 1
                emit(pos, item, "error", "", wall, 0, digest)
                break
            m = RE_RECORDED.search(out)
            m_label = re.search(r"label=(\S+)", out)
            cost, span_ms = span_cost_from_store(args.store, m[1]) if m else (0, 0)
            seen_texts.add(text)
            if is_new:
                new_since_attempt += 1
            totals["bootstrap"] += 1
            totals["paid_calls"] += 1
            emit(pos, item, "bootstrap-record", "", span_ms or wall, cost, digest,
                 answer=(m_label[1] if m_label else ""))
        else:
            run_argv = [args.auto, "run", "--artifact", live_artifact,
                        "--input", json.dumps({"ticket": text}, ensure_ascii=False),
                        "--store", args.store,
                        "--spend-cap-usd", args.cap, "--session", args.session]
            if args.deopt_mode == "frontier":
                run_argv[5:5] = ["--tier0", args.model]
            rc, out, err, wall = run_proc(run_argv)
            led_cost, _led_tokens, led_lines = ledger.delta()
            # the answer is run's last stdout line (a JSON value); best-effort decode
            answer = ""
            for ln in reversed(out.strip().splitlines() or []):
                ln = ln.strip()
                if ln:
                    try:
                        decoded = json.loads(ln)
                        answer = decoded if isinstance(decoded, str) else json.dumps(decoded)
                    except ValueError:
                        answer = ln
                    break
            proceed = RE_PROCEED.search(err)
            trip = RE_TRIP.search(err)
            if rc == 0 and proceed:
                totals["tier1"] += 1
                emit(pos, item, "tier1", proceed[1], wall, led_cost, digest, answer=answer)
                if led_cost:
                    print(f"driver: WARNING pos {pos} tier-1 answer but session ledger grew "
                          f"by {led_cost} u$ ({led_lines} lines) - investigate", file=sys.stderr)
            elif rc == 0 and trip:
                deopt_ms = int(RE_DEOPT_MS.search(err)[1]) if RE_DEOPT_MS.search(err) else wall
                if not RE_INGESTED.search(err):
                    print(f"driver: WARNING pos {pos} deopt answered but was NOT ingested - "
                          "the ratchet cannot grow from it", file=sys.stderr)
                else:
                    seen_texts.add(text)
                    if is_new:
                        new_since_attempt += 1
                totals["deopt"] += 1
                totals["paid_calls"] += 1
                emit(pos, item, "tier0-deopt", trip[1].replace("n/a", ""), deopt_ms, led_cost, digest, answer=answer)
            elif rc == 0:
                # answered without a guard marker: unguarded artifact
                tier = "tier1-unguarded" if UNGUARDED in err else "tier1"
                totals["tier1"] += 1
                emit(pos, item, tier, "", wall, led_cost, digest)
            elif rc == 3 and args.deopt_mode == "agent":
                trip_d = (trip[1].replace("n/a", "") if trip else "")
                rc2, out2, err2, wall2 = run_proc(
                    [args.auto, "record", "--store", args.store, "--",
                     args.python, args.agent, text])
                m2 = RE_RECORDED.search(out2)
                m_label2 = re.search(r"label=(\S+)", out2)
                cost2, span_ms2 = span_cost_from_store(args.store, m2[1]) if m2 else (0, 0)
                if rc2 == 0 and m2:
                    seen_texts.add(text)
                    if is_new:
                        new_since_attempt += 1
                    totals["deopt"] += 1
                    totals["paid_calls"] += 1
                    emit(pos, item, "deopt-agent-record", trip_d, span_ms2 or wall2, cost2,
                         digest, answer=(m_label2[1] if m_label2 else ""))
                else:
                    totals["error"] += 1
                    emit(pos, item, "error", trip_d, wall2, 0, digest)
                    print(f"driver: WARNING pos {pos} agent re-record failed rc={rc2}",
                          file=sys.stderr)
            elif rc == 3:
                totals["error"] += 1
                emit(pos, item, "abstain", (trip[1].replace("n/a", "") if trip else ""), wall, led_cost, digest)
                print(f"driver: WARNING pos {pos} abstained (exit 3) despite --tier0 - see stderr", file=sys.stderr)
            else:
                tail = (err.strip().splitlines() or ["no stderr"])[-1]
                totals["error"] += 1
                emit(pos, item, "error", "", wall, led_cost, digest)
                if any(s in err for s in FATAL_TIER0):
                    halted = f"tier-0 refusal at pos {pos}: {tail[:200]}"
                    print(f"driver: HALT - {halted}", file=sys.stderr)
                    break
                print(f"driver: pos {pos} run failed (rc={rc}): {tail[:200]}", file=sys.stderr)

        if new_since_attempt >= args.recompile_every:
            recompile(pos)

    sink.close()
    events.close()

    done = totals["tier1"] + totals["deopt"] + totals["bootstrap"] + totals["error"]
    tag = "FAKE " if args.dry_run else ""
    print(
        f"{tag}f6 ratchet leg: {done}/{len(stream)} positions | "
        f"bootstrap {totals['bootstrap']}, tier-1 {totals['tier1']}, deopt {totals['deopt']}, "
        f"errors {totals['error']} | paid calls {totals['paid_calls']} | "
        f"total {totals['cost']} u$ | generations {generation} | csv {args.csv}"
    )
    if halted:
        print(f"{tag}HALTED EARLY: {halted}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
