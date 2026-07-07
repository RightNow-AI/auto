#!/usr/bin/env python3
"""Micro-benchmark: in-process ``auto_py.Runner`` call latency (ADR-0024).

The last rung of the economics ladder. Loads a compiled ``.cbin`` ONCE, then
times N warm ``.answer`` calls over a JSON-lines input file and reports
p50/p95/mean microseconds per call and calls/sec from MEASURED wall time
(``time.perf_counter_ns``). Honesty rules baked in:

  * warmup calls are executed but NOT counted in the statistics;
  * the one-time load cost (module compiled once) is reported separately;
  * abstentions/errors are timed like any call and counted, never hidden;
  * the percentile method (linear interpolation between ranks) is stated below.

v0 embeds PURE artifacts only: a capability-bearing artifact is refused at load
with ``AutoError`` (see README.md). This script measures; it makes no parity or
cross-machine claim — the numbers are wall time on the machine that ran it.

usage:
    python bench.py ARTIFACT.cbin INPUTS.jsonl [--warmup W] [--iters N]

INPUTS.jsonl: one JSON value per line (the same protocol as ``auto run --stdio``).
"""

from __future__ import annotations

import argparse
import json
import sys
import time

import auto_py


def percentile(sorted_us: list[float], q: float) -> float:
    """Percentile of an already-sorted list, q in [0, 100].

    Linear interpolation between the two closest ranks (the same method numpy
    calls ``linear``). Stated explicitly so the reported number is reproducible.
    """
    if not sorted_us:
        return float("nan")
    if len(sorted_us) == 1:
        return sorted_us[0]
    rank = (q / 100.0) * (len(sorted_us) - 1)
    low = int(rank)
    high = min(low + 1, len(sorted_us) - 1)
    frac = rank - low
    return sorted_us[low] * (1.0 - frac) + sorted_us[high] * frac


def load_inputs(path: str) -> list[str]:
    """Read non-blank lines, validating each is JSON so we bench the runner,
    not a parse error inside it."""
    with open(path, "r", encoding="utf-8") as handle:
        lines = [line.strip() for line in handle if line.strip()]
    for line in lines:
        json.loads(line)  # raises on malformed input; fail loud before timing
    return lines


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("artifact", help="path to a compiled .cbin (pure artifact)")
    parser.add_argument("inputs", help="JSON-lines file: one input value per line")
    parser.add_argument("--warmup", type=int, default=200, help="untimed warmup calls")
    parser.add_argument("--iters", type=int, default=5000, help="timed calls")
    args = parser.parse_args()

    if args.iters <= 0:
        print("--iters must be positive", file=sys.stderr)
        return 2

    lines = load_inputs(args.inputs)
    if not lines:
        print("no input lines", file=sys.stderr)
        return 2

    # one-time load: the wasm module is compiled exactly once, here.
    load_start = time.perf_counter_ns()
    try:
        runner = auto_py.Runner(args.artifact)
    except auto_py.AutoError as exc:
        print(f"load refused: {exc}", file=sys.stderr)
        return 1
    load_ns = time.perf_counter_ns() - load_start

    def call(line: str) -> str:
        try:
            runner.answer(line)
            return "output"
        except auto_py.AutoAbstained:
            return "abstained"
        except auto_py.AutoError:
            return "error"

    # warmup — executed, not counted
    for i in range(args.warmup):
        call(lines[i % len(lines)])

    # timed region
    durations_us: list[float] = []
    counts = {"output": 0, "abstained": 0, "error": 0}
    wall_start = time.perf_counter_ns()
    for i in range(args.iters):
        line = lines[i % len(lines)]
        call_start = time.perf_counter_ns()
        kind = call(line)
        call_end = time.perf_counter_ns()
        durations_us.append((call_end - call_start) / 1000.0)
        counts[kind] += 1
    wall_ns = time.perf_counter_ns() - wall_start

    durations_us.sort()
    mean_us = sum(durations_us) / len(durations_us)
    calls_per_sec = args.iters / (wall_ns / 1e9)

    print(f"auto_py version:  {auto_py.version()}")
    print(f"artifact:         {args.artifact}")
    print(f"inputs:           {len(lines)} distinct line(s)")
    print(f"one-time load:    {load_ns / 1000.0:.1f} us  (module compiled once)")
    print(f"warmup calls:     {args.warmup}  (not counted)")
    print(f"timed calls:      {args.iters}")
    print(
        "outcomes:         "
        f"output={counts['output']} "
        f"abstained={counts['abstained']} "
        f"error={counts['error']}"
    )
    print(f"per-call p50:     {percentile(durations_us, 50):.3f} us")
    print(f"per-call p95:     {percentile(durations_us, 95):.3f} us")
    print(f"per-call mean:    {mean_us:.3f} us")
    print(f"throughput:       {calls_per_sec:,.0f} calls/sec")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
