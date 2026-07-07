"""AUTO-BENCH v1 collector: f*-results.json -> paper-ready markdown.

Reads a directory of per-family result files (f1-results.json ...
f6-results.json — the run-f*.sh convention; F3/F4 legs are manual per
their READMEs and may hand-write jsons in the same minimal shape) plus an
optional F6 summary markdown (the H1 ratchet curve, produced by
evals/bench/f6-stream/summarize.py), and renders the four headline
sections of evals/bench/DESIGN.md:

  H2  determinism census   (family | effectful spans | witnessed | det %)
  H3  parity-gated compression  (frontier vs compiled; parity evidence =
      agreement + eval-run id, or the refusal VERBATIM — refusals carry
      equal weight by construction)
  H4  calibrated ignorance (family x guard wire | in-dist abstained |
      ood answered)
  H1  the ratchet curve    (the F6 summary, embedded)

A missing json renders as NOT RUN — never as a guess. Nothing here is
computed from model behavior; every number is copied from a results file
that carries its own provenance (eval-run ids, ledger sessions, pins).

    python evals/bench/collect.py RESULTS_DIR [--f6-summary MD] [--out MD]
    python evals/bench/collect.py --self-test

--self-test builds labeled FAKE fixtures in a temp dir, renders them,
asserts the structure, and prints SELF-TEST OK. Fixtures are never
written into the repo.
"""

from __future__ import annotations

import argparse
import json
import os
import sys

FAMILIES = ["f1", "f2", "f3", "f4", "f5", "f6"]
FAMILY_LABELS = {
    "f1": "F1 ticket-triage",
    "f2": "F2 inbox-agent",
    "f3": "F3 field-extraction",
    "f4": "F4 policy-routing",
    "f5": "F5 summarize-strict",
    "f6": "F6 novelty-stream",
}
NOT_RUN = "NOT RUN"


def load_results(results_dir: str) -> dict[str, dict | None]:
    out: dict[str, dict | None] = {}
    for fam in FAMILIES:
        path = os.path.join(results_dir, f"{fam}-results.json")
        if os.path.isfile(path):
            with open(path, encoding="utf-8") as f:
                out[fam] = json.load(f)
        else:
            out[fam] = None
    return out


def md_cell(text: str) -> str:
    """Make arbitrary text (refusals verbatim) markdown-table safe."""
    return text.replace("|", "\\|").replace("\n", "<br>")


def fmt_num(v, suffix: str = "") -> str:
    if v is None:
        return "no data"
    if isinstance(v, float):
        return f"{v:,.1f}{suffix}"
    return f"{v:,}{suffix}"


# ------------------------------------------------------------------- H2
def h2_rows(results: dict[str, dict | None]) -> list[list[str]]:
    rows = []
    for fam in FAMILIES:
        label = FAMILY_LABELS[fam]
        r = results[fam]
        if fam == "f5":
            rows.append([label, "(uses the F2 store — census reported under F2)", "", ""])
            continue
        if r is None or "census" not in r:
            rows.append([label, NOT_RUN, "", ""])
            continue
        c = r["census"]
        wit = c.get("witnessed")
        wof = c.get("witnessed_of")
        det = c.get("deterministic")
        pct = c.get("deterministic_pct_of_witnessed")
        rows.append([
            label,
            fmt_num(c.get("effectful_spans")),
            f"{fmt_num(wit)} of {fmt_num(wof)}" if wit is not None else "no data",
            (f"{fmt_num(det)} ({pct}% of witnessed)" if det is not None else "no data"),
        ])
        for span, s in sorted((c.get("per_span") or {}).items()):
            rows.append([
                f"&nbsp;&nbsp;{fam} · {span}",
                fmt_num(s.get("groups")),
                fmt_num(s.get("witnessed")),
                (f"{fmt_num(s.get('deterministic'))} ({s.get('deterministic_pct_of_witnessed')}%)"
                 if s.get("witnessed") else "unwitnessed"),
            ])
    return rows


# ------------------------------------------------------------------- H3
def frontier_cell(r: dict | None, fam: str, results: dict[str, dict | None]) -> str:
    if fam == "f5":  # F5 rides the F2 store; its frontier side is F2's summarize span
        f2 = results.get("f2")
        ps = (((f2 or {}).get("frontier_baseline") or {}).get("per_span") or {}).get("summarize")
        if ps:
            lat = ps.get("latency_ms") or {}
            return (f"summarize span (F2 store): mean {fmt_num(ps.get('cost_usd_micros_mean'))} µ$/call, "
                    f"p50 {fmt_num(lat.get('p50'))} ms / p95 {fmt_num(lat.get('p95'))} ms")
        return "see F2 store"
    fb = (r or {}).get("frontier_baseline")
    if not fb:
        return NOT_RUN
    if "run_latency_ms" in fb:  # per-run shape (f2)
        lat, cost = fb["run_latency_ms"], fb["run_cost_usd_micros"]
        return (f"per run: mean {fmt_num(cost.get('mean'))} µ$, "
                f"p50 {fmt_num(lat.get('p50'))} ms / p95 {fmt_num(lat.get('p95'))} ms "
                f"({fmt_num(fb.get('runs'))} runs)")
    lat, cost = fb.get("latency_ms", {}), fb.get("cost_usd_micros", {})
    return (f"per call: mean {fmt_num(cost.get('mean'))} µ$, "
            f"p50 {fmt_num(lat.get('p50'))} ms / p95 {fmt_num(lat.get('p95'))} ms "
            f"({fmt_num(fb.get('calls'))} calls)")


def compiled_cell(r: dict | None) -> str:
    lp = (r or {}).get("latency_probe")
    if lp and lp.get("p50_ms") is not None:
        return (f"p50 {lp['p50_ms']} ms / p95 {lp['p95_ms']} ms ({lp.get('timer', '?')}, "
                f"one-shot process), $0 marginal")
    return "—"


def rung_rows(fam: str, r: dict | None, results: dict[str, dict | None]) -> list[list[str]]:
    label = FAMILY_LABELS[fam]
    if r is None:
        return [[label, NOT_RUN, "", ""]]
    rows: list[list[str]] = []
    sections = []
    for key in ("compile", "sub_runs"):
        for name, rung in ((r.get(key) or {})).items():
            if isinstance(rung, dict):
                sections.append((name, rung))
    if not sections:
        return [[label, frontier_cell(r, fam, results), compiled_cell(r), "no compile rungs recorded"]]
    for name, rung in sections:
        verdict = rung.get("verdict")
        agree = rung.get("agreement")
        evid_parts = []
        if verdict:
            evid_parts.append(f"verdict {verdict}")
        if isinstance(agree, dict):
            evid_parts.append(
                f"agreement measured {agree['matched']}/{agree['eligible']} = "
                f"{agree['pct_truncated']}% (declared >= {agree['declared_milli']}/1000)")
        elif isinstance(agree, str):
            evid_parts.append(agree)
        if rung.get("judged_equivalent") or rung.get("judged_not_equivalent"):
            evid_parts.append(f"judge: {rung.get('judged_equivalent', 0)} equivalent / "
                              f"{rung.get('judged_not_equivalent', 0)} not")
        if rung.get("eval_run_id"):
            evid_parts.append(f"eval run {rung['eval_run_id']}")
        refusal = rung.get("refusal_verbatim") or rung.get("refusal_or_block_verbatim")
        if refusal:
            evid_parts.append(f"REFUSED/BLOCKED verbatim: {refusal}")
        if rung.get("status"):
            evid_parts.append(str(rung["status"]))
        artifact = rung.get("artifact_id")
        compiled = (f"artifact {artifact[:12]}… — {compiled_cell(r)}" if artifact
                    else "no artifact")
        rows.append([f"{label} · {name}", frontier_cell(r, fam, results),
                     md_cell(compiled), md_cell("; ".join(evid_parts) or "no data")])
    return rows


def h3_rows(results: dict[str, dict | None]) -> list[list[str]]:
    rows = []
    for fam in FAMILIES:
        if fam == "f6":
            rows.append([FAMILY_LABELS[fam],
                         "H1 measures F6 (marginal cost per stream item); see the H1 section",
                         "", ""])
            continue
        rows.extend(rung_rows(fam, results[fam], results))
    return rows


# ------------------------------------------------------------------- H4
def h4_rows(results: dict[str, dict | None]) -> list[list[str]]:
    rows = []
    for fam in FAMILIES:
        r = results[fam]
        arts = ((r or {}).get("h4_probes") or {}).get("artifacts") or {}
        alpha = (r or {}).get("guard_alpha_milli")
        if not arts:
            if fam in ("f1", "f2", "f3", "f4"):  # families with probe legs
                rows.append([FAMILY_LABELS[fam], NOT_RUN, "", ""])
            continue
        for wire, a in sorted(arts.items()):
            if "per_probe" not in a:
                rows.append([f"{FAMILY_LABELS[fam]} · {wire}",
                             md_cell(a.get("status", NOT_RUN)), "", ""])
                continue
            rows.append([
                f"{FAMILY_LABELS[fam]} · {wire} (alpha {alpha} milli)",
                f"{a['in_dist_abstained']}/{a['in_dist_total']} (false-abstain)",
                f"{a['ood_answered']}/{a['ood_total']} (false-proceed)",
                f"other errors {a.get('other_errors', 0)}",
            ])
    return rows


# ------------------------------------------------------ refusals section
def refusal_rows(results: dict[str, dict | None]) -> list[str]:
    out = []
    for fam in FAMILIES:
        r = results[fam]
        if r is None:
            continue
        for key in ("compile", "sub_runs"):
            for name, rung in ((r.get(key) or {})).items():
                if not isinstance(rung, dict):
                    continue
                verdict = rung.get("verdict")
                refusal = rung.get("refusal_verbatim") or rung.get("refusal_or_block_verbatim")
                if refusal or (verdict and verdict != "PASS"):
                    out.append(f"- **{FAMILY_LABELS[fam]} · {name}** — verdict "
                               f"{verdict or '(none: refused before the gate)'}"
                               + (f"; verbatim:\n\n  > {md_cell(refusal)}" if refusal else ""))
    return out


# --------------------------------------------------------- spend section
def spend_rows(results: dict[str, dict | None]) -> list[str]:
    out = []
    f1 = results.get("f1")
    if f1:
        s = ((f1.get("frontier_baseline") or {}).get("cost_usd_micros") or {}).get("sum")
        if s is not None:
            out.append(f"- F1 recording: {s:,} µ$ (sum of reserved span cost attrs; "
                       f"session {f1.get('session')})")
    f2 = results.get("f2")
    if f2:
        s = ((f2.get("frontier_baseline") or {}).get("run_cost_usd_micros") or {}).get("sum")
        if s is not None:
            out.append(f"- F2 recording: {s:,} µ$ (sum of reserved span cost attrs; "
                       f"session {f2.get('session')})")
    f5 = results.get("f5")
    if f5 and f5.get("session_spend_usd_micros_total") is not None:
        out.append(f"- F5 judge: {f5['session_spend_usd_micros_total']:,} µ$ "
                   f"(ADR-0010 ledger, session {f5.get('session')})")
    return out


def table(headers: list[str], rows: list[list[str]]) -> str:
    lines = ["| " + " | ".join(headers) + " |",
             "|" + "|".join("---" for _ in headers) + "|"]
    for row in rows:
        padded = row + [""] * (len(headers) - len(row))
        lines.append("| " + " | ".join(padded) + " |")
    return "\n".join(lines)


def render(results: dict[str, dict | None], f6_summary_path: str | None) -> str:
    parts = ["# AUTO-BENCH v1 — collected results",
             "",
             "Generated by evals/bench/collect.py from per-family results files. "
             "Every number is copied from a results json carrying its own provenance "
             "(eval-run ids, ledger sessions, pins); missing families read NOT RUN. "
             "Failures and refusals are results (DESIGN.md).",
             ""]

    parts += ["## H2 — the determinism census", "",
              table(["family", "effectful spans", "witnessed", "deterministic"],
                    h2_rows(results)),
              "",
              "Witnessing per DESIGN.md: >= 2 recordings per input; fractions cover "
              "witnessed spans only, never extrapolated. Per-span sub-rows are the "
              "script-side groupby documented in the family json (`census.per_span_method`).",
              ""]

    parts += ["## H3 — parity-gated compression", "",
              table(["family · rung", "frontier (recorded = baseline)",
                     "compiled", "parity evidence / refusal"],
                    h3_rows(results)),
              "",
              "Latency probe numbers are script-timer one-shot process wall-clock "
              "(spawn + wasm compile included — the deployment-shaped number; resident "
              "and in-process floors are measured in paper/log.md waves 6/9/10).",
              ""]

    parts += ["## H4 — calibrated ignorance", "",
              table(["family · guard wire", "in-dist abstained", "OOD answered", "notes"],
                    h4_rows(results)),
              "",
              "false-proceed (OOD answered compiled) is the cardinal failure; "
              "false-abstain is wasted deopt. Probes are frozen in "
              "evals/bench/probes-f*.jsonl / evals/bench/f3-extraction/probes.jsonl.",
              ""]

    refusals = refusal_rows(results)
    parts += ["## failures and refusals (equal weight by construction)", ""]
    parts += (refusals or ["- none recorded"]) + [""]

    spends = spend_rows(results)
    parts += ["## measured spend", ""]
    parts += (spends or ["- no spend recorded"]) + [""]

    parts += ["## H1 — the ratchet curve (F6)", ""]
    if f6_summary_path and os.path.isfile(f6_summary_path):
        with open(f6_summary_path, encoding="utf-8") as f:
            parts += [f"Embedded from `{f6_summary_path}`:", "", f.read().rstrip(), ""]
    else:
        parts += [f"{NOT_RUN} — produce it with evals/bench/f6-stream/summarize.py and pass "
                  "`--f6-summary paper/evidence/f6-summary.md`.", ""]

    return "\n".join(parts) + "\n"


# ---------------------------------------------------------- self-test
def self_test() -> None:
    """Render labeled FAKE fixtures from a temp dir and assert structure.

    Everything below is FAKE and lives only in a temp dir — the self-test
    proves the rendering pipeline, never a benchmark number.
    """
    import tempfile

    tmp = tempfile.mkdtemp(prefix="bench-collect-selftest-")
    fake_f1 = {
        "family": "f1", "session": "FAKE-session", "guard_alpha_milli": 1,
        "census": {"effectful_spans": 80, "witnessed": 40, "witnessed_of": 40,
                   "deterministic": 40, "deterministic_pct_of_witnessed": 100.0},
        "frontier_baseline": {"calls": 80,
                              "latency_ms": {"p50": 700, "p95": 1100, "mean": 750.0},
                              "cost_usd_micros": {"mean": 55.0, "sum": 4400}},
        "compile": {
            "enum_v1": {"exit": 1, "verdict": None, "agreement": None,
                        "refusal_verbatim": "FAKE synthesis budget exhausted: no fitting program"},
            "distill_v1": {"exit": 0, "verdict": "PASS",
                           "agreement": {"declared_milli": 950, "matched": 40,
                                         "eligible": 40, "pct_truncated": 100.0},
                           "eval_run_id": "fake0run0id0", "artifact_id": "a" * 64,
                           "refusal_verbatim": None},
        },
        "latency_probe": {"p50_ms": 25.0, "p95_ms": 30.0, "timer": "script-timer"},
        "h4_probes": {"artifacts": {
            "v1_jaccard": {"per_probe": [], "in_dist_total": 5, "in_dist_abstained": 1,
                           "ood_total": 5, "ood_answered": 0, "other_errors": 0},
            "v2_embedding": {"status": "NOT RUN (artifact not emitted)"},
        }},
    }
    fake_f2 = {
        "family": "f2", "session": "FAKE-session",
        "census": {"effectful_spans": 160, "witnessed": 80, "witnessed_of": 80,
                   "deterministic": 63, "deterministic_pct_of_witnessed": 78.8,
                   "per_span": {"model_call/classify": {
                       "groups": 20, "witnessed": 20, "deterministic": 20,
                       "divergent": 0, "unwitnessed": 0,
                       "deterministic_pct_of_witnessed": 100.0}}},
        "frontier_baseline": {"runs": 40,
                              "run_latency_ms": {"p50": 2900, "p95": 4000, "mean": 2933.0},
                              "run_cost_usd_micros": {"mean": 188.0, "sum": 7520},
                              "per_span": {"summarize": {
                                  "calls": 40, "cost_usd_micros_mean": 60.0,
                                  "latency_ms": {"p50": 900, "p95": 1400, "mean": 950.0}}}},
        "compile": {"classify": {"exit": 1, "verdict": "FAIL", "agreement": None,
                                 "eval_run_id": "fake1run1id1",
                                 "refusal_verbatim": "FAKE emit blocked - verdict FAIL"}},
        "h4_probes": {"artifacts": {}},
    }
    fake_f5 = {
        "family": "f5", "session": "FAKE-bench-f5",
        "session_spend_usd_micros_total": 894,
        "sub_runs": {
            "a_weighted_nojudge": {"exit": 1, "verdict": "INCONCLUSIVE",
                                   "agreement": "unchecked (no judge; judged differential "
                                                "Unchecked by construction)",
                                   "refusal_or_block_verbatim": "FAKE emit blocked - INCONCLUSIVE"},
            "b_weighted_judged": {"exit": 0, "verdict": "PASS",
                                  "agreement": {"declared_milli": 800, "matched": 17,
                                                "eligible": 20, "pct_truncated": 85.0},
                                  "judged_equivalent": 0, "judged_not_equivalent": 3,
                                  "eval_run_id": "fake5run5id5", "artifact_id": "b" * 64,
                                  "refusal_or_block_verbatim": None},
            "c_mostcommon_judged": {"status": "SKIPPED (JUDGE!=1; FAKE)"},
        },
    }
    for name, data in (("f1-results.json", fake_f1), ("f2-results.json", fake_f2),
                       ("f5-results.json", fake_f5)):
        with open(os.path.join(tmp, name), "w", encoding="utf-8") as f:
            json.dump(data, f)
    f6_md = os.path.join(tmp, "f6-summary.md")
    with open(f6_md, "w", encoding="utf-8") as f:
        f.write("# FAKE F6 SUMMARY (self-test fixture)\n\nno real numbers here.\n")

    md = render(load_results(tmp), f6_md)

    checks = [
        "## H2 — the determinism census" in md,
        "## H3 — parity-gated compression" in md,
        "## H4 — calibrated ignorance" in md,
        "## H1 — the ratchet curve (F6)" in md,
        "## failures and refusals" in md,
        "F1 ticket-triage" in md,
        "78.8" in md,                       # f2 census pct flows through
        "agreement measured 17/20 = 85.0%" in md,   # f5 judged agreement
        "eval run fake5run5id5" in md,      # eval-run id rides the row
        "FAKE synthesis budget exhausted" in md,    # refusal verbatim
        NOT_RUN in md,                      # f3/f4 rows
        "FAKE F6 SUMMARY" in md,            # f6 embed
        "model_call/classify" in md,        # per-span sub-row
        "1/5 (false-abstain)" in md,        # h4 accounting
        "894" in md,                        # ledger spend
    ]
    if not all(checks):
        failed = [i for i, ok in enumerate(checks) if not ok]
        sys.stderr.write(f"SELF-TEST FAILED: checks {failed} failed\n---\n{md}\n")
        raise SystemExit(1)
    print("SELF-TEST OK")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("results_dir", nargs="?",
                        help="directory containing f*-results.json files")
    parser.add_argument("--f6-summary", help="path to the F6 summary markdown (H1)")
    parser.add_argument("--out", help="write markdown here (default: stdout)")
    parser.add_argument("--self-test", action="store_true",
                        help="render labeled FAKE fixtures from a temp dir and assert structure")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return
    if not args.results_dir:
        parser.error("results_dir is required (or use --self-test)")

    md = render(load_results(args.results_dir), args.f6_summary)
    if args.out:
        with open(args.out, "w", encoding="utf-8") as f:
            f.write(md)
        print(f"wrote {args.out}")
    else:
        try:
            sys.stdout.reconfigure(encoding="utf-8")  # windows console safety
        except AttributeError:
            pass
        print(md)


if __name__ == "__main__":
    main()
