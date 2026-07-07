"""Recording and replay of agent runs — the python side of spec/trace.md.

Record mode: every effectful call (model_call, tool_call, env_read,
memory_op, branch) is appended to a v0 JSONL trace file, one flushed line per
event. Structural ``span()`` blocks give the trace its shape.

Replay mode: tool/model/memory calls return the *recorded* outputs instead of
executing; the live run is checked call-by-call against the recording and any
difference raises :class:`ReplayDivergence`. Recorded failures re-raise as
:class:`ReplayedError`.

Honesty properties, by construction:
- the SDK never swallows exceptions: failures are recorded, then re-raised;
- ``env_read`` records a sha-256 digest + length of the value, never the
  value itself (secrets stay out of traces);
- digests are implementation-local (python compares python digests); they are
  never written to the wire — see spec/trace.md "digests";
- values must be real JSON: NaN/Infinity raise instead of corrupting the file.

Reserved span attrs (spec/trace.md §3): ``cost_usd_micros`` and ``tokens`` —
decimal u64 strings the agent may set in ``attrs`` on model/tool calls to
declare what the call's API billed; the verification harness reads them for
cost/token budget checks. The SDK never sets or computes them itself.

Task-level I/O (ADR-0025): ``Tracer(task=..., task_input=<value>)`` records
the whole-run input on the header line; ``set_task_output(value)`` appends a
``task_output`` line with the whole-run output, exactly once — a second call
is an error, never a silent last-wins. ``None`` means "not recorded" in both
positions, so a task input/output of JSON null is not recordable. Task I/O
plays no role in replay matching: it is a whole-run record, not a call.

Concurrency (record mode): span parenting is thread-local, so nesting in one
thread never sees another thread's parents; seq/span_id allocation and file
writes stay under one lock — one flushed line per event, never torn.

Concurrency (replay mode, ADR-0029): matching is concurrency-tolerant. A
live effectful call consumes the FIRST UNCONSUMED recorded span with the
same (kind, name, canonical input), atomically under the tracer lock.
Sequential runs arriving in recorded order consume exactly what the old
cursor consumed (byte-identical replay); concurrent calls match
order-independently instead of racing one shared cursor. See
:class:`Tracer` for what replay does and does not verify.
"""

from __future__ import annotations

import hashlib
import json
import os
import secrets
import threading
import time
from collections import deque
from typing import Any, Callable, Optional

FORMAT_VERSION = 0
__version__ = "0.1.0"
SDK_NAME = f"auto-sdk-python/{__version__}"

_EFFECTFUL_KINDS = ("model_call", "tool_call", "env_read", "memory_op", "branch")


def canonical_json(value: Any) -> str:
    """Canonical JSON within this SDK: sorted keys, compact, NaN rejected."""
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False
    )


def digest_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


class ReplayDivergence(RuntimeError):
    """The live run made a different call than the recording."""


class ReplayedError(RuntimeError):
    """The recorded call failed; replay faithfully re-raises the failure."""


class _ParentStack(threading.local):
    """Per-thread span parent stack. ``__init__`` re-runs in every thread
    that touches the instance, so each thread starts with an empty stack —
    a thread never inherits another thread's parents."""

    def __init__(self) -> None:
        self.stack: list[int] = []


class _SpanContext:
    """Structural span: ``with tracer.span("step"):``. Emitted at close."""

    def __init__(self, tracer: "Tracer", name: str, attrs: Optional[dict]) -> None:
        self._tracer = tracer
        self._name = name
        self._attrs = attrs or {}
        self._span_id: Optional[int] = None
        self._seq: Optional[int] = None
        self._parent: Optional[int] = None
        self._started = 0

    def __enter__(self) -> "_SpanContext":
        t = self._tracer
        self._span_id, self._seq, self._parent = t._open_span()
        t._parents.stack.append(self._span_id)
        self._started = t._now_ms()
        return self

    def __exit__(self, exc_type, exc, _tb) -> bool:
        t = self._tracer
        t._parents.stack.pop()
        error = None if exc is None else f"{exc_type.__name__}: {exc}"
        t._emit_span(
            span_id=self._span_id,
            parent_span_id=self._parent,
            seq=self._seq,
            kind="span",
            name=self._name,
            input_value={},
            output=None,
            error=error,
            started_at_ms=self._started,
            attrs=self._attrs,
        )
        return False  # never swallow


class Tracer:
    """Record one agent run, or replay one. See module docs.

    Record mode is thread-safe: span parenting is thread-local (nesting in
    one thread never sees another thread's parents; a fresh thread starts
    unparented — spans do not inherit parents across ``Thread`` boundaries),
    while seq/span_id allocation and file writes share one lock, one flushed
    line per event.

    Replay matching is concurrency-tolerant (ADR-0029): each live effectful
    call consumes the FIRST UNCONSUMED recorded span with the same
    (kind, name, canonical input); the take is atomic under the tracer
    lock. Sequential runs arriving in recorded order consume exactly the
    old cursor's sequence; concurrent calls match order-independently.
    Replay therefore verifies the MULTISET of effectful calls and their
    recorded I/O, not arrival order (cross-trace order comparison lives in
    rust `auto-trace::replay::compare`). Caveat — divergent duplicates:
    the same (kind, name, input) recorded twice with DIFFERENT outputs is
    assigned in recorded order by ARRIVAL; under concurrency arrival order
    is a race by construction, so which call receives which recorded
    output is undefined. Unconsumed recorded spans at exit stay silent
    (the pre-ADR-0029 behavior, kept); ``replay_remaining`` reports them.
    """

    def __init__(
        self,
        task: str,
        path: Optional[str] = None,
        replay: Optional[str] = None,
        now_ms: Optional[Callable[[], int]] = None,
        task_input: Any = None,
    ) -> None:
        self._task = task
        self._now_ms = now_ms or (lambda: time.time_ns() // 1_000_000)
        self._lock = threading.Lock()
        self._seq = 0
        self._next_span_id = 1
        self._parents = _ParentStack()
        self._closed = False
        self._trace_id = secrets.token_hex(16)
        self._replay_events: Optional[list[dict]] = None
        # (kind, name, canonical input) -> unconsumed indices into
        # _replay_events, each queue in recorded (seq) order (ADR-0029)
        self._replay_index: Optional[dict[tuple[str, str, str], deque[int]]] = None
        self._replay_consumed = 0
        self._file = None
        self._task_output_set = False

        if replay is not None:
            self._replay_events = _load_effectful(replay)
            self._replay_index = {}
            for i, rec in enumerate(self._replay_events):
                key = (rec["kind"], rec["name"], canonical_json(rec["input"]))
                self._replay_index.setdefault(key, deque()).append(i)

        out_path = path or os.environ.get("AUTO_TRACE_FILE")
        if replay is None and not out_path:
            raise ValueError(
                "record mode needs a trace path: pass path= or set AUTO_TRACE_FILE"
            )
        if out_path:
            self._file = open(out_path, "a", encoding="utf-8")
            header = {
                "v": FORMAT_VERSION,
                "t": "trace",
                "trace_id": self._trace_id,
                "task": self._task,
                "started_at_ms": self._now_ms(),
                "sdk": SDK_NAME,
                "attrs": {},
            }
            if task_input is not None:
                # None means "not recorded" — the field appears on the wire
                # only when a task input was actually given (ADR-0025)
                header["task_input"] = task_input
            self._emit_line(header)

    # -- lifecycle -------------------------------------------------------

    def __enter__(self) -> "Tracer":
        return self

    def __exit__(self, *_exc) -> bool:
        self.close()
        return False

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self._file is not None:
            self._file.close()

    @property
    def trace_id(self) -> str:
        return self._trace_id

    @property
    def replay_remaining(self) -> int:
        """Recorded effectful calls the live run has not consumed (replay mode).

        Unconsumed spans at exit are not an error (pre-ADR-0029 behavior,
        kept); this property is how a caller observes them."""
        if self._replay_events is None:
            return 0
        return len(self._replay_events) - self._replay_consumed

    # -- structural spans --------------------------------------------------

    def span(self, name: str, attrs: Optional[dict] = None) -> _SpanContext:
        return _SpanContext(self, name, attrs)

    # -- task-level output (ADR-0025) ---------------------------------------

    def set_task_output(self, value: Any) -> None:
        """Declare the whole-run output, exactly once. A second call raises —
        the recorded output is what the agent declared, never a silent
        last-wins. ``None`` raises too: None means "not recorded", so a task
        output of JSON null is not recordable. The declaration is appended as
        its own ``task_output`` line (the header is already on disk); replay
        ignores it."""
        if value is None:
            raise ValueError(
                "task output None is not recordable; leave set_task_output "
                "uncalled to record no output"
            )
        if self._file is None:
            # replay without a capture path records nothing, but the
            # exactly-once contract still holds
            with self._lock:
                if self._task_output_set:
                    raise RuntimeError("set_task_output called twice")
                self._task_output_set = True
            return
        if self._closed:
            raise RuntimeError("tracer is closed")
        # serialize before taking the once-flag: NaN/Infinity raise here,
        # recording nothing and burning nothing
        line = canonical_json(
            {
                "v": FORMAT_VERSION,
                "t": "task_output",
                "trace_id": self._trace_id,
                "output": value,
                "recorded_at_ms": self._now_ms(),
            }
        )
        with self._lock:
            if self._task_output_set:
                raise RuntimeError("set_task_output called twice")
            self._task_output_set = True
            self._file.write(line + "\n")
            self._file.flush()

    # -- effectful calls ---------------------------------------------------

    def tool_call(
        self,
        name: str,
        input: Any,
        fn: Optional[Callable[[], Any]] = None,
        attrs: Optional[dict] = None,
    ) -> Any:
        """Record (or replay) one tool invocation. `fn` is a zero-arg closure
        performing the real call; `input` declares what went in. Reserved
        ``attrs`` keys ``cost_usd_micros`` / ``tokens`` (decimal u64 strings)
        declare what the call billed — see spec/trace.md §3."""
        return self._leaf("tool_call", name, input, fn, attrs)

    def model_call(
        self,
        name: str,
        input: Any,
        fn: Optional[Callable[[], Any]] = None,
        attrs: Optional[dict] = None,
    ) -> Any:
        """Record (or replay) one model invocation. Reserved ``attrs`` keys
        ``cost_usd_micros`` / ``tokens`` (decimal u64 strings) declare what
        the call billed — see spec/trace.md §3."""
        return self._leaf("model_call", name, input, fn, attrs)

    def memory_op(
        self,
        op: str,
        key: str,
        fn: Optional[Callable[[], Any]] = None,
        value: Any = None,
        attrs: Optional[dict] = None,
    ) -> Any:
        """Witness an agent memory-store operation. op: read|write|append."""
        if op not in ("read", "write", "append"):
            raise ValueError(f"unknown memory op {op!r}")
        input_value: dict[str, Any] = {"key": key}
        if op != "read":
            input_value["value"] = value
        return self._leaf("memory_op", op, input_value, fn, attrs)

    def env_read(self, var: str) -> Optional[str]:
        """Read an environment variable, recording only a digest + length —
        never the value. Replay verifies the digest and returns the LIVE
        value (a changed environment is a divergence)."""
        value = os.environ.get(var)
        payload = (
            None
            if value is None
            else {"digest": digest_hex(value), "len": len(value)}
        )
        if self._replay_events is not None:
            rec = self._replay_take("env_read", var, {})
            recorded = rec.get("output")
            if recorded != payload:
                raise ReplayDivergence(
                    f"env_read({var!r}): environment changed since recording"
                )
        span_id, seq, parent = self._open_span()
        started = self._now_ms()
        self._emit_span(
            span_id=span_id,
            parent_span_id=parent,
            seq=seq,
            kind="env_read",
            name=var,
            input_value={},
            output=payload,
            error=None,
            started_at_ms=started,
            attrs={},
        )
        return value

    def branch(self, name: str, input: Any, decision: Any) -> Any:
        """Witness a decision. Replay verifies the live decision equals the
        recorded one and returns it."""
        if self._replay_events is not None:
            rec = self._replay_take("branch", name, input)
            if canonical_json(rec.get("output")) != canonical_json(decision):
                raise ReplayDivergence(
                    f"branch({name!r}): live decision {canonical_json(decision)} "
                    f"differs from recorded {canonical_json(rec.get('output'))}"
                )
        span_id, seq, parent = self._open_span()
        started = self._now_ms()
        self._emit_span(
            span_id=span_id,
            parent_span_id=parent,
            seq=seq,
            kind="branch",
            name=name,
            input_value=input,
            output=decision,
            error=None,
            started_at_ms=started,
            attrs={},
        )
        return decision

    # -- internals ---------------------------------------------------------

    def _leaf(
        self,
        kind: str,
        name: str,
        input_value: Any,
        fn: Optional[Callable[[], Any]],
        attrs: Optional[dict],
    ) -> Any:
        if self._replay_events is not None:
            rec = self._replay_take(kind, name, input_value)
            if rec.get("error"):
                self._record_leaf(kind, name, input_value, None, rec["error"], attrs)
                raise ReplayedError(rec["error"])
            output = rec.get("output")
            self._record_leaf(kind, name, input_value, output, None, attrs)
            return output

        output = None
        error = None
        started = self._now_ms()
        span_id, seq, parent = self._open_span()
        try:
            if fn is not None:
                output = fn()
            return output
        except Exception as exc:
            error = f"{type(exc).__name__}: {exc}"
            raise
        finally:
            self._emit_span(
                span_id=span_id,
                parent_span_id=parent,
                seq=seq,
                kind=kind,
                name=name,
                input_value=input_value,
                output=None if error is not None else output,
                error=error,
                started_at_ms=started,
                attrs=attrs or {},
            )

    def _record_leaf(
        self,
        kind: str,
        name: str,
        input_value: Any,
        output: Any,
        error: Optional[str],
        attrs: Optional[dict],
    ) -> None:
        span_id, seq, parent = self._open_span()
        self._emit_span(
            span_id=span_id,
            parent_span_id=parent,
            seq=seq,
            kind=kind,
            name=name,
            input_value=input_value,
            output=output,
            error=error,
            started_at_ms=self._now_ms(),
            attrs=attrs or {},
        )

    def _replay_take(self, kind: str, name: str, input_value: Any) -> dict:
        """Consume the first unconsumed recorded span matching (kind, name,
        canonical input) — ADR-0029. In-order arrival consumes in recorded
        order (identical to the old sequential cursor); concurrent arrivals
        match order-independently. Divergent duplicates: the same key
        recorded twice with different outputs is assigned in recorded order
        by arrival; under concurrency arrival order is a race."""
        live_input = canonical_json(input_value)
        key = (kind, name, live_input)
        events = self._replay_events
        index = self._replay_index
        assert events is not None and index is not None
        with self._lock:
            queue = index.get(key)
            if queue:
                i = queue.popleft()
                if not queue:
                    del index[key]
                self._replay_consumed += 1
                return events[i]
            # no match: snapshot what remains unconsumed, in recorded order
            remaining = sorted(i for q in index.values() for i in q)
        if not remaining:
            raise ReplayDivergence(
                f"recording exhausted: live run called {kind}({name!r}) "
                f"after all {len(events)} recorded calls"
            )
        same_call = sum(
            1
            for i in remaining
            if events[i]["kind"] == kind and events[i]["name"] == name
        )
        if same_call:
            raise ReplayDivergence(
                f"{kind}({name!r}): input differs from recording: live input "
                f"{_snippet(live_input)} matches none of the {same_call} "
                f"unconsumed recorded {kind}({name!r}) call(s); "
                f"{len(remaining)} recorded call(s) remain unconsumed"
            )
        listed = ", ".join(
            f"recorded {events[i]['kind']}({events[i]['name']!r})"
            for i in remaining[:8]
        )
        if len(remaining) > 8:
            listed += f", and {len(remaining) - 8} more"
        raise ReplayDivergence(
            f"no unconsumed recorded call matches live {kind}({name!r}) "
            f"input {_snippet(live_input)}; unconsumed: {listed}"
        )

    def _open_span(self) -> tuple[int, int, Optional[int]]:
        stack = self._parents.stack  # thread-local: no lock needed
        parent = stack[-1] if stack else None
        with self._lock:
            self._seq += 1
            seq = self._seq
            span_id = self._next_span_id
            self._next_span_id += 1
        return span_id, seq, parent

    def _emit_span(
        self,
        *,
        span_id: int,
        parent_span_id: Optional[int],
        seq: int,
        kind: str,
        name: str,
        input_value: Any,
        output: Any,
        error: Optional[str],
        started_at_ms: int,
        attrs: dict,
    ) -> None:
        duration = max(0, self._now_ms() - started_at_ms)
        self._emit_line(
            {
                "v": FORMAT_VERSION,
                "t": "span",
                "trace_id": self._trace_id,
                "span_id": span_id,
                "parent_span_id": parent_span_id,
                "seq": seq,
                "kind": kind,
                "name": name,
                "input": input_value,
                "output": output,
                "error": error,
                "started_at_ms": started_at_ms,
                "duration_ms": duration,
                "attrs": {str(k): str(v) for k, v in attrs.items()},
            }
        )

    def _emit_line(self, obj: dict) -> None:
        if self._file is None:
            return  # replay without a capture path records nothing
        if self._closed:
            raise RuntimeError("tracer is closed")
        line = canonical_json(obj)
        with self._lock:
            self._file.write(line + "\n")
            self._file.flush()


def _snippet(text: str, limit: int = 80) -> str:
    """Truncate canonical-JSON for divergence messages (ASCII, bounded)."""
    return text if len(text) <= limit else text[: limit - 3] + "..."


def _load_effectful(path: str) -> list[dict]:
    """Load the effectful spans of a recorded trace, in seq order."""
    spans: list[dict] = []
    saw_header = False
    with open(path, encoding="utf-8") as f:
        for raw in f:
            raw = raw.strip()
            if not raw:
                continue
            obj = json.loads(raw)
            if obj.get("v") != FORMAT_VERSION:
                raise ValueError(
                    f"unsupported trace format version {obj.get('v')!r}"
                )
            tag = obj.get("t")
            if tag == "trace":
                saw_header = True
            elif tag == "span":
                if obj.get("kind") in _EFFECTFUL_KINDS:
                    spans.append(obj)
            elif tag == "task_output":
                pass  # whole-run record; plays no role in replay (ADR-0025)
            else:
                raise ValueError(f"unknown line type {tag!r}")
    if not saw_header:
        raise ValueError(f"{path}: not a trace file (no header line)")
    spans.sort(key=lambda s: s["seq"])
    return spans
