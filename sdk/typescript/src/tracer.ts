/**
 * Recording and replay of agent runs — the typescript side of spec/trace.md.
 *
 * Mirrors the python SDK contract exactly: same v0 JSONL emission format,
 * same replay semantics, same honesty rules —
 * - exceptions are recorded, then re-thrown (never swallowed);
 * - envRead records a sha-256 digest + length, never the value;
 * - NaN/Infinity/undefined throw instead of corrupting the trace;
 * - digests are implementation-local (this SDK compares its own digests);
 *   they are never written to the wire — see spec/trace.md "digests".
 *
 * Concurrency (record mode): span parenting rides AsyncLocalStorage, so
 * overlapping awaited spans (Promise.all over traced `span()` calls) each
 * see their own parent chain, and leaf calls parent to their own wrapper.
 * Sync callers work unchanged (no store -> root span). seq/span_id are
 * plain instance counters — the event loop is single-threaded, no lock
 * needed — and every line is one synchronous append, so lines never
 * interleave.
 *
 * Replay matching is concurrency-tolerant (ADR-0029): each live effectful
 * call consumes the FIRST UNCONSUMED recorded span with the same
 * (kind, name, canonical input) — the take is synchronous on the event
 * loop, so overlapping async calls never race a shared cursor. Sequential
 * runs arriving in recorded order consume exactly the old cursor's
 * sequence (byte-identical replay); concurrent calls match
 * order-independently. Replay therefore verifies the MULTISET of effectful
 * calls and their recorded I/O, not arrival order. Caveat — divergent
 * duplicates: the same (kind, name, input) recorded twice with DIFFERENT
 * outputs is assigned in recorded order by ARRIVAL; under concurrency
 * arrival order is a race by construction, so which call receives which
 * recorded output is undefined. Unconsumed recorded spans at exit stay
 * silent (the pre-ADR-0029 behavior, kept); `replayRemaining` reports
 * them.
 *
 * Reserved span attrs (spec/trace.md §3): `cost_usd_micros` and `tokens` —
 * decimal u64 strings the agent may set in `attrs` on model/tool calls to
 * declare what the call's API billed; the verification harness reads them
 * for cost/token budget checks. The SDK never sets or computes them itself.
 *
 * Task-level I/O (ADR-0025): the `taskInput` constructor option records the
 * whole-run input on the header line; `setTaskOutput(value)` appends a
 * `task_output` line with the whole-run output, exactly once — a second call
 * throws, never a silent last-wins. null/undefined mean "not recorded" in
 * both positions, so a task input/output of JSON null is not recordable.
 * Task I/O plays no role in replay matching: it is a whole-run record, not
 * a call.
 */

import { AsyncLocalStorage } from "node:async_hooks";
import { createHash, randomBytes } from "node:crypto";
import { appendFileSync, readFileSync } from "node:fs";

export const FORMAT_VERSION = 0;
export const SDK_VERSION = "0.1.0";
export const SDK_NAME = `auto-sdk-typescript/${SDK_VERSION}`;

const EFFECTFUL_KINDS = new Set([
  "model_call",
  "tool_call",
  "env_read",
  "memory_op",
  "branch",
]);

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** The live run diverged from the recorded trace. */
export class ReplayDivergence extends Error {}

/** The recorded call failed; replay faithfully re-raises the failure. */
export class ReplayedError extends Error {}

function sortValue(value: unknown): Json {
  if (value === null) return null;
  switch (typeof value) {
    case "boolean":
    case "string":
      return value;
    case "number":
      if (!Number.isFinite(value)) {
        throw new Error("NaN/Infinity is not JSON; refusing to record it");
      }
      return value;
    case "object": {
      if (Array.isArray(value)) return value.map(sortValue);
      const out: { [key: string]: Json } = {};
      for (const key of Object.keys(value as object).sort()) {
        const entry = (value as Record<string, unknown>)[key];
        if (entry === undefined) continue; // JSON.stringify drops these silently; be explicit
        out[key] = sortValue(entry);
      }
      return out;
    }
    default:
      throw new Error(`value of type ${typeof value} is not JSON; refusing to record it`);
  }
}

/** Canonical JSON within this SDK: sorted keys, compact, NaN rejected. */
export function canonicalJson(value: unknown): string {
  return JSON.stringify(sortValue(value));
}

export function digestHex(text: string): string {
  return createHash("sha256").update(text, "utf8").digest("hex");
}

interface RecordedSpan {
  v: number;
  t: string;
  trace_id: string;
  span_id: number;
  parent_span_id: number | null;
  seq: number;
  kind: string;
  name: string;
  input: unknown;
  output: unknown;
  error: string | null;
  started_at_ms: number;
  duration_ms: number;
  attrs: Record<string, string>;
}

function loadEffectful(path: string): RecordedSpan[] {
  const spans: RecordedSpan[] = [];
  let sawHeader = false;
  for (const raw of readFileSync(path, "utf8").split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const obj = JSON.parse(line);
    if (obj.v !== FORMAT_VERSION) {
      throw new Error(`unsupported trace format version ${obj.v}`);
    }
    if (obj.t === "trace") {
      sawHeader = true;
    } else if (obj.t === "span") {
      if (EFFECTFUL_KINDS.has(obj.kind)) spans.push(obj as RecordedSpan);
    } else if (obj.t === "task_output") {
      // whole-run record; plays no role in replay (ADR-0025)
    } else {
      throw new Error(`unknown line type ${JSON.stringify(obj.t)}`);
    }
  }
  if (!sawHeader) throw new Error(`${path}: not a trace file (no header line)`);
  spans.sort((a, b) => a.seq - b.seq);
  return spans;
}

function errorString(e: unknown): string {
  if (e instanceof Error) return `${e.constructor.name}: ${e.message}`;
  return `Error: ${String(e)}`;
}

/** Truncate canonical-JSON for divergence messages (bounded). */
function snippet(text: string, limit = 80): string {
  return text.length <= limit ? text : text.slice(0, limit - 3) + "...";
}

/**
 * (kind, name, canonical input) -> unconsumed indices into the recorded
 * events, each queue in recorded (seq) order (ADR-0029).
 */
function buildReplayIndex(events: RecordedSpan[]): Map<string, number[]> {
  const index = new Map<string, number[]>();
  events.forEach((rec, i) => {
    const key = JSON.stringify([rec.kind, rec.name, canonicalJson(rec.input)]);
    const queue = index.get(key);
    if (queue) queue.push(i);
    else index.set(key, [i]);
  });
  return index;
}

export interface TracerOptions {
  task: string;
  /** trace output; defaults to AUTO_TRACE_FILE. Required in record mode. */
  path?: string;
  /** path of a recorded trace to replay against */
  replay?: string;
  /** clock override for tests */
  nowMs?: () => number;
  /**
   * whole-run task input (ADR-0025); recorded on the header line.
   * null/undefined mean "not recorded" — the field never hits the wire.
   */
  taskInput?: unknown;
}

export class Tracer {
  readonly traceId: string;
  private readonly task: string;
  private readonly nowMs: () => number;
  private readonly path: string | null;
  private readonly replayEvents: RecordedSpan[] | null;
  private readonly replayIndex: Map<string, number[]> | null;
  private replayConsumed = 0;
  private seq = 0;
  private nextSpanId = 1;
  /** parent chain of the current async context; no store = root */
  private readonly parents = new AsyncLocalStorage<readonly number[]>();
  private closed = false;
  private taskOutputSet = false;

  constructor(options: TracerOptions) {
    this.task = options.task;
    this.nowMs = options.nowMs ?? (() => Date.now());
    this.traceId = randomBytes(16).toString("hex");
    this.replayEvents = options.replay != null ? loadEffectful(options.replay) : null;
    this.replayIndex = this.replayEvents === null ? null : buildReplayIndex(this.replayEvents);

    const path = options.path ?? process.env.AUTO_TRACE_FILE ?? null;
    if (this.replayEvents === null && !path) {
      throw new Error("record mode needs a trace path: pass path or set AUTO_TRACE_FILE");
    }
    this.path = path;
    if (this.path) {
      const header: Record<string, unknown> = {
        v: FORMAT_VERSION,
        t: "trace",
        trace_id: this.traceId,
        task: this.task,
        started_at_ms: this.nowMs(),
        sdk: SDK_NAME,
        attrs: {},
      };
      if (options.taskInput !== null && options.taskInput !== undefined) {
        // null/undefined mean "not recorded" — the field appears on the
        // wire only when a task input was actually given (ADR-0025)
        header.task_input = options.taskInput;
      }
      this.emitLine(header);
    }
  }

  /**
   * Declare the whole-run output, exactly once (ADR-0025). A second call
   * throws — the recorded output is what the agent declared, never a
   * silent last-wins. null/undefined throw too: they mean "not recorded",
   * so a task output of JSON null is not recordable. The declaration is
   * appended as its own `task_output` line (the header is already on
   * disk); replay ignores it.
   */
  setTaskOutput(value: unknown): void {
    if (value === null || value === undefined) {
      throw new Error(
        "task output null/undefined is not recordable; leave setTaskOutput " +
          "uncalled to record no output",
      );
    }
    if (this.taskOutputSet) {
      throw new Error("setTaskOutput called twice");
    }
    if (this.path !== null) {
      if (this.closed) throw new Error("tracer is closed");
      // serialize before taking the once-flag: NaN/Infinity throw here,
      // recording nothing and burning nothing
      const line = canonicalJson({
        v: FORMAT_VERSION,
        t: "task_output",
        trace_id: this.traceId,
        output: value,
        recorded_at_ms: this.nowMs(),
      });
      appendFileSync(this.path, line + "\n", "utf8");
    }
    // replay without a capture path records nothing, but the exactly-once
    // contract still holds
    this.taskOutputSet = true;
  }

  close(): void {
    this.closed = true; // appendFileSync leaves nothing buffered
  }

  /**
   * Recorded effectful calls the live run has not consumed (replay mode).
   * Unconsumed spans at exit are not an error (pre-ADR-0029 behavior,
   * kept); this getter is how a caller observes them.
   */
  get replayRemaining(): number {
    if (this.replayEvents === null) return 0;
    return this.replayEvents.length - this.replayConsumed;
  }

  /**
   * Structural span around `fn` (sync or promise-returning). `fn` runs in
   * an async context whose parent chain ends at this span, so overlapping
   * concurrent spans each parent their own leaf calls.
   */
  span<T>(name: string, fn: () => T): T {
    const [spanId, seq, parent] = this.openSpan();
    const started = this.nowMs();
    let finished = false;
    const finish = (error: string | null) => {
      if (finished) return;
      finished = true;
      this.emitSpan(spanId, parent, seq, "span", name, {}, null, error, started, {});
    };
    const chain = this.parents.getStore() ?? [];
    try {
      const result = this.parents.run([...chain, spanId], fn);
      if (result instanceof Promise) {
        return result.then(
          (value) => {
            finish(null);
            return value;
          },
          (err) => {
            finish(errorString(err));
            throw err;
          },
        ) as T;
      }
      finish(null);
      return result;
    } catch (err) {
      finish(errorString(err));
      throw err;
    }
  }

  /**
   * Record (or replay) one tool invocation. Reserved `attrs` keys
   * `cost_usd_micros` / `tokens` (decimal u64 strings) declare what the
   * call billed — see spec/trace.md §3.
   */
  toolCall<T>(name: string, input: unknown, fn?: () => T, attrs?: Record<string, string>): T {
    return this.leaf("tool_call", name, input, fn, attrs);
  }

  /**
   * Record (or replay) one model invocation. Reserved `attrs` keys
   * `cost_usd_micros` / `tokens` (decimal u64 strings) declare what the
   * call billed — see spec/trace.md §3.
   */
  modelCall<T>(name: string, input: unknown, fn?: () => T, attrs?: Record<string, string>): T {
    return this.leaf("model_call", name, input, fn, attrs);
  }

  /** Witness an agent memory-store operation. op: read|write|append. */
  memoryOp<T>(op: string, key: string, fn?: () => T, value?: unknown): T {
    if (op !== "read" && op !== "write" && op !== "append") {
      throw new Error(`unknown memory op ${JSON.stringify(op)}`);
    }
    const input: Record<string, unknown> = { key };
    if (op !== "read") input.value = value ?? null;
    return this.leaf("memory_op", op, input, fn, undefined);
  }

  /**
   * Read an environment variable, recording only a digest + length — never
   * the value. Replay verifies the digest and returns the LIVE value.
   */
  envRead(name: string): string | null {
    const value = process.env[name] ?? null;
    const payload =
      value === null ? null : { digest: digestHex(value), len: value.length };
    if (this.replayEvents !== null) {
      const rec = this.replayTake("env_read", name, {});
      if (canonicalJson(rec.output) !== canonicalJson(payload)) {
        throw new ReplayDivergence(
          `envRead(${JSON.stringify(name)}): environment changed since recording`,
        );
      }
    }
    const [spanId, seq, parent] = this.openSpan();
    this.emitSpan(spanId, parent, seq, "env_read", name, {}, payload, null, this.nowMs(), {});
    return value;
  }

  /** Witness a decision. Replay verifies it matches the recording. */
  branch<T>(name: string, input: unknown, decision: T): T {
    if (this.replayEvents !== null) {
      const rec = this.replayTake("branch", name, input);
      if (canonicalJson(rec.output) !== canonicalJson(decision)) {
        throw new ReplayDivergence(
          `branch(${JSON.stringify(name)}): live decision ${canonicalJson(decision)} ` +
            `differs from recorded ${canonicalJson(rec.output)}`,
        );
      }
    }
    const [spanId, seq, parent] = this.openSpan();
    this.emitSpan(spanId, parent, seq, "branch", name, input, decision, null, this.nowMs(), {});
    return decision;
  }

  // -- internals ---------------------------------------------------------

  private leaf<T>(
    kind: string,
    name: string,
    input: unknown,
    fn: (() => T) | undefined,
    attrs: Record<string, string> | undefined,
  ): T {
    if (this.replayEvents !== null) {
      const rec = this.replayTake(kind, name, input);
      if (rec.error) {
        this.recordLeaf(kind, name, input, null, rec.error, attrs);
        throw new ReplayedError(rec.error);
      }
      this.recordLeaf(kind, name, input, rec.output, null, attrs);
      return rec.output as T;
    }

    const [spanId, seq, parent] = this.openSpan();
    const started = this.nowMs();
    let finished = false;
    const finish = (output: unknown, error: string | null) => {
      if (finished) return;
      finished = true;
      this.emitSpan(
        spanId,
        parent,
        seq,
        kind,
        name,
        input,
        error === null ? output : null,
        error,
        started,
        attrs ?? {},
      );
    };
    try {
      const result = fn ? fn() : (null as T);
      if (result instanceof Promise) {
        return result.then(
          (value) => {
            finish(value, null);
            return value;
          },
          (err) => {
            finish(null, errorString(err));
            throw err;
          },
        ) as T;
      }
      finish(result, null);
      return result;
    } catch (err) {
      finish(null, errorString(err));
      throw err;
    }
  }

  private recordLeaf(
    kind: string,
    name: string,
    input: unknown,
    output: unknown,
    error: string | null,
    attrs: Record<string, string> | undefined,
  ): void {
    const [spanId, seq, parent] = this.openSpan();
    this.emitSpan(spanId, parent, seq, kind, name, input, output, error, this.nowMs(), attrs ?? {});
  }

  /**
   * Consume the first unconsumed recorded span matching (kind, name,
   * canonical input) — ADR-0029. In-order arrival consumes in recorded
   * order (identical to the old sequential cursor); concurrent arrivals
   * match order-independently. Divergent duplicates: the same key recorded
   * twice with different outputs is assigned in recorded order by arrival;
   * under concurrency arrival order is a race.
   */
  private replayTake(kind: string, name: string, input: unknown): RecordedSpan {
    const events = this.replayEvents;
    const index = this.replayIndex;
    if (events === null || index === null) throw new Error("not in replay mode");
    const liveInput = canonicalJson(input);
    const key = JSON.stringify([kind, name, liveInput]);
    const queue = index.get(key);
    if (queue && queue.length > 0) {
      const i = queue.shift() as number;
      if (queue.length === 0) index.delete(key);
      this.replayConsumed += 1;
      return events[i];
    }
    // no match: report what remains unconsumed, in recorded order
    const remaining = [...index.values()].flat().sort((a, b) => a - b);
    if (remaining.length === 0) {
      throw new ReplayDivergence(
        `recording exhausted: live run called ${kind}(${JSON.stringify(name)}) ` +
          `after all ${events.length} recorded calls`,
      );
    }
    const sameCall = remaining.filter(
      (i) => events[i].kind === kind && events[i].name === name,
    ).length;
    if (sameCall > 0) {
      throw new ReplayDivergence(
        `${kind}(${JSON.stringify(name)}): input differs from recording: live input ` +
          `${snippet(liveInput)} matches none of the ${sameCall} unconsumed recorded ` +
          `${kind}(${JSON.stringify(name)}) call(s); ${remaining.length} recorded ` +
          `call(s) remain unconsumed`,
      );
    }
    let listed = remaining
      .slice(0, 8)
      .map((i) => `recorded ${events[i].kind}(${JSON.stringify(events[i].name)})`)
      .join(", ");
    if (remaining.length > 8) listed += `, and ${remaining.length - 8} more`;
    throw new ReplayDivergence(
      `no unconsumed recorded call matches live ${kind}(${JSON.stringify(name)}) ` +
        `input ${snippet(liveInput)}; unconsumed: ${listed}`,
    );
  }

  private openSpan(): [number, number, number | null] {
    this.seq += 1;
    const spanId = this.nextSpanId;
    this.nextSpanId += 1;
    const chain = this.parents.getStore();
    const parent = chain && chain.length ? chain[chain.length - 1] : null;
    return [spanId, this.seq, parent];
  }

  private emitSpan(
    spanId: number,
    parent: number | null,
    seq: number,
    kind: string,
    name: string,
    input: unknown,
    output: unknown,
    error: string | null,
    startedAtMs: number,
    attrs: Record<string, string>,
  ): void {
    const cleanAttrs: Record<string, string> = {};
    for (const [k, v] of Object.entries(attrs)) cleanAttrs[String(k)] = String(v);
    this.emitLine({
      v: FORMAT_VERSION,
      t: "span",
      trace_id: this.traceId,
      span_id: spanId,
      parent_span_id: parent,
      seq,
      kind,
      name,
      input: input === undefined ? null : input,
      output: output === undefined ? null : output,
      error,
      started_at_ms: startedAtMs,
      duration_ms: Math.max(0, this.nowMs() - startedAtMs),
      attrs: cleanAttrs,
    });
  }

  private emitLine(obj: unknown): void {
    if (this.path === null) return; // replay without a capture path records nothing
    if (this.closed) throw new Error("tracer is closed");
    appendFileSync(this.path, canonicalJson(obj) + "\n", "utf8");
  }
}
