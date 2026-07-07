"""Tests for the recording/replay tracer. stdlib + pytest only; no network."""

import json
import threading
from collections import Counter

import pytest

from auto_sdk import ReplayDivergence, ReplayedError, Tracer, digest_hex


def read_lines(path):
    with open(path, encoding="utf-8") as f:
        return [json.loads(line) for line in f if line.strip()]


def record_sample(path, monkeypatch):
    """One recorded run used by several tests."""
    monkeypatch.setenv("SAMPLE_VAR", "hello")
    with Tracer(task="toy", path=str(path)) as t:
        with t.span("step"):
            t.tool_call("add", {"a": 1, "b": 2}, lambda: 3)
            t.env_read("SAMPLE_VAR")
        t.branch("size", {"n": 3}, "small")
        t.memory_op("write", "k", lambda: None, value="v")
    return path


def test_record_produces_valid_v0_jsonl(tmp_path, monkeypatch):
    path = record_sample(tmp_path / "t.jsonl", monkeypatch)
    lines = read_lines(path)

    header, spans = lines[0], lines[1:]
    assert header["t"] == "trace"
    assert header["v"] == 0
    assert header["task"] == "toy"
    assert len(header["trace_id"]) == 32

    assert {s["t"] for s in spans} == {"span"}
    assert all(s["trace_id"] == header["trace_id"] for s in spans)
    seqs = sorted(s["seq"] for s in spans)
    assert len(set(seqs)) == len(seqs), "seq values must be unique"
    kinds = sorted(s["kind"] for s in spans)
    assert kinds == ["branch", "env_read", "memory_op", "span", "tool_call"]


def test_nesting_parents_and_seq_order(tmp_path, monkeypatch):
    path = record_sample(tmp_path / "t.jsonl", monkeypatch)
    spans = {s["kind"]: s for s in read_lines(path)[1:]}

    wrapper = spans["span"]
    child = spans["tool_call"]
    assert child["parent_span_id"] == wrapper["span_id"]
    # parent opens first (smaller seq) even though its line is written later
    assert wrapper["seq"] < child["seq"]
    assert spans["branch"]["parent_span_id"] is None


def test_env_read_records_digest_never_value(tmp_path, monkeypatch):
    monkeypatch.setenv("SECRET_TOKEN", "hunter2-super-secret")
    path = tmp_path / "t.jsonl"
    with Tracer(task="toy", path=str(path)) as t:
        assert t.env_read("SECRET_TOKEN") == "hunter2-super-secret"
    raw = path.read_text(encoding="utf-8")
    assert "hunter2-super-secret" not in raw
    span = read_lines(path)[1]
    assert span["output"] == {
        "digest": digest_hex("hunter2-super-secret"),
        "len": len("hunter2-super-secret"),
    }


def test_unset_env_var_records_null(tmp_path, monkeypatch):
    monkeypatch.delenv("NO_SUCH_VAR", raising=False)
    path = tmp_path / "t.jsonl"
    with Tracer(task="toy", path=str(path)) as t:
        assert t.env_read("NO_SUCH_VAR") is None
    assert read_lines(path)[1]["output"] is None


def test_errors_are_recorded_and_reraised(tmp_path):
    path = tmp_path / "t.jsonl"

    def boom():
        raise ValueError("nope")

    with Tracer(task="toy", path=str(path)) as t:
        with pytest.raises(ValueError):
            t.tool_call("explode", {}, boom)
    span = read_lines(path)[1]
    assert span["error"] == "ValueError: nope"
    assert span["output"] is None


def test_nan_is_rejected_not_recorded(tmp_path):
    path = tmp_path / "t.jsonl"
    with Tracer(task="toy", path=str(path)) as t:
        with pytest.raises(ValueError):
            t.tool_call("bad", {"x": float("nan")}, lambda: 1)


def test_record_mode_requires_a_path(monkeypatch):
    monkeypatch.delenv("AUTO_TRACE_FILE", raising=False)
    with pytest.raises(ValueError):
        Tracer(task="toy")


def test_path_from_environment(tmp_path, monkeypatch):
    path = tmp_path / "env.jsonl"
    monkeypatch.setenv("AUTO_TRACE_FILE", str(path))
    with Tracer(task="toy") as t:
        t.tool_call("noop", {}, lambda: None)
    assert read_lines(path)[0]["task"] == "toy"


def test_replay_substitutes_recorded_outputs(tmp_path, monkeypatch):
    original = record_sample(tmp_path / "orig.jsonl", monkeypatch)

    def must_not_run():
        raise AssertionError("replay must not execute the live call")

    with Tracer(task="toy", replay=str(original)) as t:
        with t.span("step"):
            assert t.tool_call("add", {"a": 1, "b": 2}, must_not_run) == 3
            assert t.env_read("SAMPLE_VAR") == "hello"
        assert t.branch("size", {"n": 3}, "small") == "small"
        t.memory_op("write", "k", must_not_run, value="v")
        assert t.replay_remaining == 0


def test_replay_can_record_its_own_trace(tmp_path, monkeypatch):
    original = record_sample(tmp_path / "orig.jsonl", monkeypatch)
    replay_out = tmp_path / "replay.jsonl"
    with Tracer(task="toy", replay=str(original), path=str(replay_out)) as t:
        with t.span("step"):
            t.tool_call("add", {"a": 1, "b": 2}, lambda: 99)  # substituted: 3
            t.env_read("SAMPLE_VAR")
        t.branch("size", {"n": 3}, "small")
        t.memory_op("write", "k", None, value="v")
    spans = read_lines(replay_out)[1:]
    add = next(s for s in spans if s["kind"] == "tool_call")
    assert add["output"] == 3, "replay trace records the substituted output"


def test_replay_divergence_on_different_call(tmp_path, monkeypatch):
    original = record_sample(tmp_path / "orig.jsonl", monkeypatch)
    with Tracer(task="toy", replay=str(original)) as t:
        with t.span("step"):
            with pytest.raises(ReplayDivergence, match="recorded tool_call"):
                t.model_call("add", {"a": 1, "b": 2}, lambda: 3)


def test_replay_divergence_on_input_change(tmp_path, monkeypatch):
    original = record_sample(tmp_path / "orig.jsonl", monkeypatch)
    with Tracer(task="toy", replay=str(original)) as t:
        with t.span("step"):
            with pytest.raises(ReplayDivergence, match="input differs"):
                t.tool_call("add", {"a": 1, "b": 999}, lambda: 3)


def test_replay_divergence_on_changed_env(tmp_path, monkeypatch):
    original = record_sample(tmp_path / "orig.jsonl", monkeypatch)
    monkeypatch.setenv("SAMPLE_VAR", "changed!")
    with Tracer(task="toy", replay=str(original)) as t:
        with t.span("step"):
            t.tool_call("add", {"a": 1, "b": 2}, lambda: 3)
            with pytest.raises(ReplayDivergence, match="environment changed"):
                t.env_read("SAMPLE_VAR")


def test_replay_divergence_on_changed_decision(tmp_path, monkeypatch):
    original = record_sample(tmp_path / "orig.jsonl", monkeypatch)
    with Tracer(task="toy", replay=str(original)) as t:
        with t.span("step"):
            t.tool_call("add", {"a": 1, "b": 2}, lambda: 3)
            t.env_read("SAMPLE_VAR")
        with pytest.raises(ReplayDivergence, match="live decision"):
            t.branch("size", {"n": 3}, "huge")


def test_replay_divergence_on_exhaustion(tmp_path, monkeypatch):
    original = record_sample(tmp_path / "orig.jsonl", monkeypatch)
    with Tracer(task="toy", replay=str(original)) as t:
        with t.span("step"):
            t.tool_call("add", {"a": 1, "b": 2}, lambda: 3)
            t.env_read("SAMPLE_VAR")
        t.branch("size", {"n": 3}, "small")
        t.memory_op("write", "k", None, value="v")
        with pytest.raises(ReplayDivergence, match="exhausted"):
            t.tool_call("extra", {}, lambda: 1)


def test_recorded_error_replays_as_replayed_error(tmp_path):
    original = tmp_path / "orig.jsonl"

    def boom():
        raise RuntimeError("db down")

    with Tracer(task="toy", path=str(original)) as t:
        with pytest.raises(RuntimeError):
            t.tool_call("db.query", {"q": "select 1"}, boom)

    with Tracer(task="toy", replay=str(original)) as t:
        with pytest.raises(ReplayedError, match="db down"):
            t.tool_call("db.query", {"q": "select 1"}, lambda: "fine")


def test_replay_rejects_non_trace_file(tmp_path):
    bogus = tmp_path / "bogus.jsonl"
    bogus.write_text('{"v":0,"t":"span","kind":"tool_call","seq":1}\n', encoding="utf-8")
    with pytest.raises(ValueError, match="no header"):
        Tracer(task="toy", replay=str(bogus))


def test_task_io_recorded_on_wire(tmp_path):
    path = tmp_path / "t.jsonl"
    with Tracer(task="toy", path=str(path), task_input={"doc": "d"}) as t:
        t.tool_call("noop", {}, lambda: None)
        t.set_task_output({"summary": "s", "words": 2})
    lines = read_lines(path)
    header = lines[0]
    assert header["task_input"] == {"doc": "d"}
    outputs = [l for l in lines if l["t"] == "task_output"]
    assert len(outputs) == 1
    assert outputs[0]["output"] == {"summary": "s", "words": 2}
    assert outputs[0]["trace_id"] == header["trace_id"]
    assert outputs[0]["v"] == 0
    assert isinstance(outputs[0]["recorded_at_ms"], int)


def test_task_output_twice_is_an_error(tmp_path):
    with Tracer(task="toy", path=str(tmp_path / "t.jsonl")) as t:
        t.set_task_output("first")
        with pytest.raises(RuntimeError, match="twice"):
            t.set_task_output("second")


def test_task_output_none_is_rejected(tmp_path):
    path = tmp_path / "t.jsonl"
    with Tracer(task="toy", path=str(path)) as t:
        with pytest.raises(ValueError, match="not recordable"):
            t.set_task_output(None)
    assert all(l["t"] != "task_output" for l in read_lines(path))


def test_wire_without_task_io_is_byte_identical(tmp_path):
    """The hard ADR-0025 invariant: a recording that never uses task I/O
    emits byte-for-byte what the pre-ADR-0025 SDK emitted (modulo the random
    trace id, substituted below)."""
    path = tmp_path / "t.jsonl"
    with Tracer(task="toy", path=str(path), now_ms=lambda: 1000) as t:
        t.tool_call("add", {"a": 1}, lambda: 3)
    raw = path.read_text(encoding="utf-8").replace(t.trace_id, "T" * 32)
    expected = (
        '{"attrs":{},"sdk":"auto-sdk-python/0.1.0","started_at_ms":1000,'
        '"t":"trace","task":"toy","trace_id":"' + "T" * 32 + '","v":0}\n'
        '{"attrs":{},"duration_ms":0,"error":null,"input":{"a":1},'
        '"kind":"tool_call","name":"add","output":3,"parent_span_id":null,'
        '"seq":1,"span_id":1,"started_at_ms":1000,"t":"span",'
        '"trace_id":"' + "T" * 32 + '","v":0}\n'
    )
    assert raw == expected


def test_replay_ignores_task_output_lines(tmp_path):
    """Task I/O plays no role in replay matching (ADR-0025)."""
    original = tmp_path / "orig.jsonl"
    with Tracer(task="toy", path=str(original), task_input="in") as t:
        t.tool_call("add", {"a": 1}, lambda: 3)
        t.set_task_output("out")
    with Tracer(task="toy", replay=str(original)) as t:
        assert t.tool_call("add", {"a": 1}, lambda: 99) == 3
        assert t.replay_remaining == 0


# -- concurrency-tolerant replay (ADR-0029) --------------------------------


def test_sequential_replay_byte_identical_pin(tmp_path, monkeypatch):
    """Explicit ADR-0029 pin: a sequential run replayed in recorded order
    re-records a byte-identical trace (modulo the random trace ids) — the
    first-unconsumed matcher consumes exactly what the old cursor did."""
    monkeypatch.setenv("SAMPLE_VAR", "hello")
    original = tmp_path / "orig.jsonl"
    replayed = tmp_path / "replay.jsonl"

    def run(tracer):
        with tracer.span("step"):
            tracer.tool_call("add", {"a": 1, "b": 2}, lambda: 3)
            tracer.env_read("SAMPLE_VAR")
        tracer.branch("size", {"n": 3}, "small")
        tracer.memory_op("write", "k", lambda: None, value="v")

    with Tracer(task="toy", path=str(original), now_ms=lambda: 1000) as t:
        run(t)
    with Tracer(
        task="toy", replay=str(original), path=str(replayed), now_ms=lambda: 1000
    ) as r:
        run(r)
        assert r.replay_remaining == 0
    orig_raw = original.read_text(encoding="utf-8").replace(t.trace_id, "T" * 32)
    repl_raw = replayed.read_text(encoding="utf-8").replace(r.trace_id, "T" * 32)
    assert orig_raw == repl_raw


def test_replay_matches_reordered_sequential_calls(tmp_path):
    """First-unconsumed matching is order-independent: same calls in a
    different order all match, exhausting the recording."""
    original = tmp_path / "orig.jsonl"
    with Tracer(task="toy", path=str(original)) as t:
        t.tool_call("work", {"i": 0}, lambda: "r0")
        t.tool_call("work", {"i": 1}, lambda: "r1")
    with Tracer(task="toy", replay=str(original)) as r:
        assert r.tool_call("work", {"i": 1}, lambda: "live") == "r1"
        assert r.tool_call("work", {"i": 0}, lambda: "live") == "r0"
        assert r.replay_remaining == 0


def test_threaded_replay_interleaved_identical_names(tmp_path):
    """Two threads replay interleaved identical-name calls with distinct
    inputs: every call must match regardless of arrival order (ADR-0029);
    under the old shared cursor this raised ReplayDivergence racily."""
    n_calls = 25
    original = tmp_path / "orig.jsonl"
    with Tracer(task="conc", path=str(original)) as t:
        for idx in range(2):
            for j in range(n_calls):
                t.tool_call(
                    "work", {"t": idx, "j": j}, lambda idx=idx, j=j: f"out-{idx}-{j}"
                )

    barrier = threading.Barrier(2)
    failures: list[BaseException] = []
    with Tracer(task="conc", replay=str(original)) as r:

        def worker(idx: int) -> None:
            try:
                barrier.wait(timeout=30)
                for j in range(n_calls):
                    out = r.tool_call("work", {"t": idx, "j": j}, lambda: "live")
                    assert out == f"out-{idx}-{j}"
            except BaseException as exc:  # collected for the main thread
                failures.append(exc)

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(2)]
        for th in threads:
            th.start()
        for th in threads:
            th.join()
        assert failures == []
        assert r.replay_remaining == 0


def test_replay_divergence_names_unknown_call_and_remains(tmp_path, monkeypatch):
    """No unconsumed match: the divergence names the live (kind, name),
    an input snippet, and what remains unconsumed."""
    original = record_sample(tmp_path / "orig.jsonl", monkeypatch)
    with Tracer(task="toy", replay=str(original)) as t:
        with pytest.raises(ReplayDivergence) as exc:
            t.tool_call("nonexistent", {"z": 1}, lambda: 1)
    msg = str(exc.value)
    assert "no unconsumed recorded call matches live tool_call('nonexistent')" in msg
    assert '{"z":1}' in msg
    assert "recorded tool_call('add')" in msg
    assert "recorded env_read('SAMPLE_VAR')" in msg


def test_divergent_duplicates_assigned_in_arrival_order(tmp_path):
    """Caveat pin (ADR-0029): the same (kind, name, input) recorded twice
    with DIFFERENT outputs is assigned in recorded order by ARRIVAL —
    deterministic for sequential replay (program order = arrival order)."""
    original = tmp_path / "orig.jsonl"
    with Tracer(task="toy", path=str(original)) as t:
        t.tool_call("dup", {}, lambda: "first")
        t.tool_call("dup", {}, lambda: "second")
    with Tracer(task="toy", replay=str(original)) as r:
        assert r.tool_call("dup", {}, lambda: "live") == "first"
        assert r.tool_call("dup", {}, lambda: "live") == "second"
        assert r.replay_remaining == 0


def test_divergent_duplicates_race_conserves_recordings(tmp_path):
    """Caveat pin, concurrent half: under concurrency WHICH call gets WHICH
    duplicate output is a race by construction — but each recorded span is
    consumed exactly once, so the outputs received are exactly the outputs
    recorded."""
    original = tmp_path / "orig.jsonl"
    with Tracer(task="toy", path=str(original)) as t:
        t.tool_call("dup", {}, lambda: "first")
        t.tool_call("dup", {}, lambda: "second")

    barrier = threading.Barrier(2)
    outputs: list[str] = []
    failures: list[BaseException] = []
    with Tracer(task="toy", replay=str(original)) as r:

        def worker() -> None:
            try:
                barrier.wait(timeout=30)
                outputs.append(r.tool_call("dup", {}, lambda: "live"))
            except BaseException as exc:
                failures.append(exc)

        threads = [threading.Thread(target=worker) for _ in range(2)]
        for th in threads:
            th.start()
        for th in threads:
            th.join()
        assert failures == []
        assert sorted(outputs) == ["first", "second"]
        assert r.replay_remaining == 0


def test_unconsumed_recording_at_exit_is_silent(tmp_path, monkeypatch):
    """End-of-replay pin: exiting with unconsumed recorded spans is NOT an
    error (pre-ADR-0029 behavior, kept); replay_remaining reports them."""
    original = record_sample(tmp_path / "orig.jsonl", monkeypatch)
    with Tracer(task="toy", replay=str(original)) as t:
        with t.span("step"):
            assert t.tool_call("add", {"a": 1, "b": 2}, lambda: 99) == 3
        assert t.replay_remaining == 3
    # close() raised nothing; the leftover recorded calls were simply unused


def test_concurrent_record_thread_local_parenting(tmp_path):
    """8 threads x 25 nested spans each: every line valid v0, seqs exactly
    1..=total, and every tool_call parents to ITS OWN thread's wrapper."""
    n_threads, spans_per_thread = 8, 25
    path = tmp_path / "conc.jsonl"
    barrier = threading.Barrier(n_threads)
    failures: list[BaseException] = []

    with Tracer(task="conc", path=str(path)) as t:

        def worker(idx: int) -> None:
            try:
                barrier.wait(timeout=30)
                for j in range(spans_per_thread):
                    with t.span(f"wrap-{idx}"):
                        t.tool_call(f"tool-{idx}", {"j": j}, lambda j=j: j)
            except BaseException as exc:  # collected for the main thread
                failures.append(exc)

        threads = [
            threading.Thread(target=worker, args=(i,)) for i in range(n_threads)
        ]
        for th in threads:
            th.start()
        for th in threads:
            th.join()

    assert failures == []
    lines = read_lines(path)  # every line parses: no torn/interleaved writes
    header, spans = lines[0], lines[1:]
    total = n_threads * spans_per_thread * 2  # wrapper + tool_call per iteration
    assert header["t"] == "trace"
    assert len(spans) == total
    assert all(s["t"] == "span" and s["v"] == 0 for s in spans)
    assert all(s["trace_id"] == header["trace_id"] for s in spans)

    assert sorted(s["seq"] for s in spans) == list(range(1, total + 1))
    assert len({s["span_id"] for s in spans}) == total

    by_id = {s["span_id"]: s for s in spans}
    children = [s for s in spans if s["kind"] == "tool_call"]
    wrappers = [s for s in spans if s["kind"] == "span"]
    assert len(children) == len(wrappers) == total // 2
    for child in children:
        idx = child["name"].removeprefix("tool-")
        wrapper = by_id[child["parent_span_id"]]
        assert wrapper["kind"] == "span"
        assert (
            wrapper["name"] == f"wrap-{idx}"
        ), "child must parent to its own thread's wrapper"
        assert wrapper["seq"] < child["seq"]
    # workers hold no cross-thread parents: every wrapper is a root span,
    # and each wrapper has exactly one child (per-iteration pairing)
    assert all(w["parent_span_id"] is None for w in wrappers)
    parent_counts = Counter(c["parent_span_id"] for c in children)
    assert len(parent_counts) == len(wrappers)
    assert set(parent_counts.values()) == {1}
