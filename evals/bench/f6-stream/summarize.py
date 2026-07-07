#!/usr/bin/env python3
"""AUTO-BENCH v1 - F6 novelty-stream summarizer: per-position CSV -> markdown.

Windowed means (default window 25): cost/item, latency/item, tier-1 hit
fraction, deopt/bootstrap counts, new-distinct counts - with the frozen
shift positions (50 / 120 / 200) annotated and an ASCII cost bar per
window. Totals: ratchet-leg spend, the ARITHMETIC control total, and the
ratio between them.

The control rule (same rule as `driver.py --control`, stated once more so
this file stands alone): `auto run` requires --artifact - there is no
artifact-less pure-tier-0 mode - so the pure-frontier control is computed
from measured reality, never re-fired: each stream position is priced at
the mean measured tier-0 cost of its distinct ticket (its
bootstrap-record / tier0-deopt rows in the ratchet CSV); a ticket never
paid there (possible only via a guard false-proceed on first appearance)
is priced at the global mean paid cost and counted as estimated. If
--control-csv (a `driver.py --control` output) is supplied its totals are
used verbatim instead; otherwise the rule runs here on the ratchet CSV.

No plotting deps, stdlib only, pure read - safe anywhere. A DRY-RUN input
is detected from the CSV header comments and loudly labeled FAKE.
"""

from __future__ import annotations

import argparse
import csv
import sys

SHIFTS = {
    50: "shift 1 @50: +security",
    120: "shift 2 @120: +onboarding",
    200: "shift 3 @200: +billing-fraud phrasing",
}
PAID_TIERS = ("bootstrap-record", "tier0-deopt")
BAR_WIDTH = 24
SPARK = " .:-=+*#%@"


def read_csv(path: str) -> tuple[list[dict], list[str]]:
    with open(path, encoding="utf-8") as f:
        lines = f.readlines()
    comments = [l[1:].strip() for l in lines if l.startswith("#")]
    rows = list(csv.DictReader([l for l in lines if not l.startswith("#")]))
    return rows, comments


def to_int(s: str | None) -> int:
    try:
        return int(s or 0)
    except ValueError:
        return 0


def derive_control(rows: list[dict]) -> tuple[int, int, int]:
    """(total_micros, n_positions, n_estimated) under the frozen rule."""
    paid: dict[str, list[int]] = {}
    for r in rows:
        cost = to_int(r["cost_usd_micros"])
        if r["tier"] in PAID_TIERS and cost > 0:
            paid.setdefault(r["text_sha12"], []).append(cost)
    if not paid:
        return 0, len(rows), len(rows)
    all_costs = [c for v in paid.values() for c in v]
    mean_cost = round(sum(all_costs) / len(all_costs))
    total = estimated = 0
    for r in rows:
        obs = paid.get(r["text_sha12"])
        if obs:
            total += round(sum(obs) / len(obs))
        else:
            total += mean_cost
            estimated += 1
    return total, len(rows), estimated


def bar(value: float, top: float, width: int = BAR_WIDTH) -> str:
    if top <= 0:
        return ""
    return "#" * max(0, round(width * value / top))


def spark(values: list[float]) -> str:
    top = max(values) if values else 0
    if top <= 0:
        return SPARK[0] * len(values)
    return "".join(SPARK[min(len(SPARK) - 1, round((v / top) * (len(SPARK) - 1)))] for v in values)


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--csv", required=True, help="ratchet leg per-position CSV (driver.py output)")
    p.add_argument("--events", default=None, help="events CSV (default: <csv>.events.csv if present)")
    p.add_argument("--control-csv", default=None, help="driver.py --control output; else derived here")
    p.add_argument("--out", default=None, help="markdown out (default: stdout)")
    p.add_argument("--window", type=int, default=25)
    args = p.parse_args(argv)

    rows, comments = read_csv(args.csv)
    if not rows:
        print("error: no data rows in the CSV", file=sys.stderr)
        return 2
    fake = any("DRY-RUN" in c for c in comments)

    events = []
    events_path = args.events or (args.csv + ".events.csv")
    try:
        events, _ = read_csv(events_path)
    except OSError:
        pass

    lines: list[str] = []
    w = lines.append
    w("# F6 novelty-stream - the ratchet curve (H1)")
    w("")
    if fake:
        w("**DRY-RUN - FAKE DATA.** Every number below comes from the driver's labeled fake walk")
        w("(pinned constants, no paid calls, no binaries). This file demonstrates the pipeline")
        w("shape only; rerun the driver without `--dry-run` for the measurement.")
        w("")
    w(f"Source: `{args.csv}` ({len(rows)} positions). Frozen shifts: 50 (+security),")
    w("120 (+onboarding), 200 (+billing-fraud phrasing). Window = "
      f"{args.window} positions.")
    w("")

    # ---------------------------------------------------------- windows ---
    w("## per-window decay")
    w("")
    w("| window | positions | mean cost u$/item | mean latency ms | tier-1 % | deopt | bootstrap | new distinct | cost bar |")
    w("|---|---|---|---|---|---|---|---|---|")
    window_rows = []
    seen_before = 0
    for start in range(0, len(rows), args.window):
        chunk = rows[start:start + args.window]
        n = len(chunk)
        cost = sum(to_int(r["cost_usd_micros"]) for r in chunk) / n
        lat = sum(to_int(r["latency_ms"]) for r in chunk) / n
        t1 = sum(1 for r in chunk if r["tier"].startswith("tier1")) / n * 100
        deopt = sum(1 for r in chunk if r["tier"] == "tier0-deopt")
        boot = sum(1 for r in chunk if r["tier"] == "bootstrap-record")
        end_distinct = to_int(chunk[-1]["distinct_seen"])
        new_distinct = max(0, end_distinct - seen_before)
        seen_before = max(seen_before, end_distinct)
        first_pos = to_int(chunk[0]["pos"])
        last_pos = to_int(chunk[-1]["pos"])
        marks = [note for at, note in SHIFTS.items() if first_pos <= at <= last_pos]
        window_rows.append((first_pos, last_pos, cost, lat, t1, deopt, boot, new_distinct, marks))
    top_cost = max(wr[2] for wr in window_rows)
    for first_pos, last_pos, cost, lat, t1, deopt, boot, new_distinct, marks in window_rows:
        label = f"{first_pos}-{last_pos}" + (f" <- {'; '.join(marks)}" if marks else "")
        w(f"| {label} | {last_pos - first_pos + 1} | {cost:.1f} | {lat:.0f} | {t1:.0f}% "
          f"| {deopt} | {boot} | {new_distinct} | `{bar(cost, top_cost)}` |")
    w("")
    w("cost/item sparkline (one char per window, scaled to the peak window):")
    w("")
    w(f"    [{spark([wr[2] for wr in window_rows])}]  peak = {top_cost:.1f} u$/item")
    w("")

    # ----------------------------------------------------------- events ---
    if events:
        w("## recompile events")
        w("")
        w("| pos | event | generation | distinct witnesses | detail |")
        w("|---|---|---|---|---|")
        for e in events:
            w(f"| {e['pos']} | {e['event']} | {e['generation']} | {e['distinct_witnesses']} "
              f"| {e.get('detail', '')[:90]} |")
        w("")

    # ----------------------------------------------------------- totals ---
    ratchet_total = sum(to_int(r["cost_usd_micros"]) for r in rows)
    paid_calls = sum(1 for r in rows if r["tier"] in PAID_TIERS)
    tier1_total = sum(1 for r in rows if r["tier"].startswith("tier1"))
    errors = sum(1 for r in rows if r["tier"] in ("error", "abstain"))
    if args.control_csv:
        crows, ccomments = read_csv(args.control_csv)
        control_total = sum(to_int(r["cost_usd_micros"]) for r in crows)
        estimated = sum(to_int(r.get("estimated")) for r in crows)
        control_src = f"`{args.control_csv}` (driver --control output)"
        fake = fake or any("DRY-RUN" in c for c in ccomments)
    else:
        control_total, _, estimated = derive_control(rows)
        control_src = "derived here from the ratchet CSV (same frozen rule)"

    w("## totals")
    w("")
    tag = " **(FAKE)**" if fake else ""
    w(f"- ratchet leg total: **{ratchet_total} u$** over {len(rows)} positions "
      f"({paid_calls} paid calls, {tier1_total} tier-1 answers, {errors} errors/abstains){tag}")
    w(f"- arithmetic control total (every position bought at its measured tier-0 price): "
      f"**{control_total} u$** ({estimated} positions estimated from the mean; {control_src}){tag}")
    if ratchet_total > 0:
        w(f"- control / ratchet cost ratio: **{control_total / ratchet_total:.1f}x**{tag}")
    else:
        w("- control / ratchet cost ratio: undefined (ratchet total is 0)")
    w("")

    # ---------------------------------------------------------- honesty ---
    w("## honesty notes")
    w("")
    w("- The control is ARITHMETIC (rule in this file's docstring): `auto run` has no")
    w("  artifact-less tier-0 mode, so the flat-cost line is computed from the measured")
    w("  per-ticket tier-0 prices, never re-fired as a second paid pass.")
    w("- tier-1 rows measure a one-shot `auto run` wall time (process spawn + wasm compile")
    w("  included). The resident runner (spec/runtime.md par.9) amortizes that; this leg")
    w("  reports the honest one-shot number.")
    w("- Guards are lexical (trigram distance, spec/runtime.md par.2): a tier-1 hit means")
    w("  in-calibration, not verified-correct. Correctness under distribution shift is")
    w("  H4's measurement (false-proceed rate), not this CSV's.")
    w("- Tier-0 deopt answers are unverified reference authority folded in by the next")
    w("  recompile gate; refused/inconclusive recompiles appear in the events table and")
    w("  their cost stays in the curve - failures are results.")
    text = "\n".join(lines) + "\n"

    if args.out:
        with open(args.out, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
        print(f"summary -> {args.out}" + (" (FAKE dry-run data)" if fake else ""))
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
