# ADR-0029: concurrency-tolerant replay matching

status: accepted · scope: `sdk/python`, `sdk/typescript`
(replay matcher only), `spec/trace.md` §8. Recording untouched; wire format
untouched; task-level I/O keeps playing no role in replay (ADR-0025); rust
`auto-trace::replay::compare` untouched.

## context

Record mode has been concurrent since post-spine wave 1: python span
parenting is thread-local under one write lock, typescript parenting rides
AsyncLocalStorage. But both replay matchers consumed ONE shared recorded
order through a cursor, so concurrent effectful calls during replay raced
it — whichever call lost the interleaving drew the wrong recorded slot and
raised `ReplayDivergence` nondeterministically. Recorded in open-questions
("replay of concurrent runs"); traces of concurrent agents existed and
could not be replayed. This ADR closes that.

## decision

**Matching rule.** A live effectful call consumes the **first unconsumed**
recorded span with the same `(kind, name, canonical input)` — a per-key
FIFO over recorded `seq` order, taken atomically (python: under the tracer
lock; typescript: synchronously on the event loop).

- Sequential runs arriving in recorded order consume exactly the old
  cursor's sequence; a replay that re-records emits a byte-identical trace
  (pinned by golden tests in both SDKs).
- Concurrent arrivals match order-independently: interleaving no longer
  races a shared cursor.
- **No match** raises `ReplayDivergence` naming the live `(kind, name)`, a
  canonical-input snippet, and what remains unconsumed. Three flavors:
  same `(kind, name)` unconsumed but no input match → "input differs";
  nothing matching at all → the unconsumed remainder is listed; recording
  fully consumed → the pre-existing "recording exhausted" wording, kept.
- **End of replay.** Unconsumed recorded spans at exit stay **silent** —
  verified pre-existing behavior (`close()` never checked consumption),
  mirrored deliberately; `replay_remaining` / `replayRemaining` reports
  the count.
- **Divergent duplicates caveat.** the same (kind, name, input) recorded
  twice with DIFFERENT outputs is assigned in recorded order by ARRIVAL;
  under concurrency arrival order is a race by construction — which
  concurrent call receives which recorded output is undefined. Sequential
  runs arrive in program order and are unaffected. Each recording is
  consumed exactly once either way (pinned: deterministic sequential
  assignment; conservation under concurrent replay).
- Replay stays opt-in exactly as before; recording is untouched.

## alternatives considered

**Per-context sub-cursors** (one cursor per recording thread / async
task, matched to a replaying context): needs a stable context identity on
the wire; thread ids and async-task ids are not stable across runs, and
inventing a correlation id is a recording format change that solves
nothing the key does not already solve. Rejected.

**Subtree alignment via `parent_span_id`** (match within the recorded
subtree of the live call's structural parent): heavier, ambiguous for
root-level leaves and same-named wrappers, and couples replay correctness
to the agent's incidental wrapper structure. Rejected.

**Dual semantics — strict order for sequential runs, order-free only when
concurrency is detected**: replay cannot reliably detect that a recording
or a live run was concurrent (no wire marker), and two matching semantics
are worse than one. Rejected.

**Status quo (sequential-only, documented)**: fails the actual need —
record mode already supports concurrent agents, so their traces exist and
were unreplayable. Rejected.

## consequences

- Replay now verifies the **multiset** of effectful calls and their
  recorded I/O, not arrival order: a sequential run that reorders its
  calls replays clean where it used to diverge at the first reordered
  call. Cross-trace ORDER comparison still lives in rust
  (`auto-trace::replay::compare`, `seq`-order walk) and determinism
  analysis — unchanged here.
- Divergent-duplicate assignment under concurrency is a race by
  construction (caveat above, stated verbatim in spec/trace.md §8).
- O(recorded effectful spans) index memory per replay-mode tracer.
- Rust `auto-trace::replay::compare` zips two traces' effectful spans
  positionally in `seq` order, so comparing two runs of a CONCURRENT agent
  whose interleavings differ reports a false `SignatureMismatch`. Same
  limitation, different component; not touched here — recorded in
  open-questions, owner `auto-trace`.
