/** Tests for the recording/replay tracer. node:test + node stdlib; no deps, no network. */

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { ReplayDivergence, ReplayedError, Tracer, digestHex } from "../src/tracer.ts";

function readLines(path: string): any[] {
  return readFileSync(path, "utf8")
    .split("\n")
    .filter((l) => l.trim())
    .map((l) => JSON.parse(l));
}

function withDir<T>(fn: (dir: string) => T): T {
  const dir = mkdtempSync(join(tmpdir(), "auto-sdk-ts-"));
  const cleanup = () => rmSync(dir, { recursive: true, force: true });
  let result: T;
  try {
    result = fn(dir);
  } catch (err) {
    cleanup();
    throw err;
  }
  if (result instanceof Promise) {
    return result.finally(cleanup) as T;
  }
  cleanup();
  return result;
}

function recordSample(path: string): void {
  process.env.SAMPLE_VAR = "hello";
  const t = new Tracer({ task: "toy", path });
  try {
    t.span("step", () => {
      assert.equal(
        t.toolCall("add", { a: 1, b: 2 }, () => 3),
        3,
      );
      t.envRead("SAMPLE_VAR");
    });
    t.branch("size", { n: 3 }, "small");
    t.memoryOp("write", "k", () => null, "v");
  } finally {
    t.close();
  }
}

test("record produces valid v0 jsonl", () => {
  withDir((dir) => {
    const path = join(dir, "t.jsonl");
    recordSample(path);
    const [header, ...spans] = readLines(path);
    assert.equal(header.t, "trace");
    assert.equal(header.v, 0);
    assert.equal(header.task, "toy");
    assert.equal(header.trace_id.length, 32);
    assert.ok(spans.every((s) => s.t === "span" && s.trace_id === header.trace_id));
    const seqs = spans.map((s) => s.seq);
    assert.equal(new Set(seqs).size, seqs.length, "seq values must be unique");
    assert.deepEqual(
      spans.map((s) => s.kind).sort(),
      ["branch", "env_read", "memory_op", "span", "tool_call"],
    );
  });
});

test("nesting: parent opens first even though its line is written later", () => {
  withDir((dir) => {
    const path = join(dir, "t.jsonl");
    recordSample(path);
    const spans = Object.fromEntries(readLines(path).slice(1).map((s) => [s.kind, s]));
    assert.equal(spans.tool_call.parent_span_id, spans.span.span_id);
    assert.ok(spans.span.seq < spans.tool_call.seq);
    assert.equal(spans.branch.parent_span_id, null);
  });
});

test("envRead records digest + length, never the value", () => {
  withDir((dir) => {
    const path = join(dir, "t.jsonl");
    process.env.SECRET_TOKEN = "hunter2-super-secret";
    const t = new Tracer({ task: "toy", path });
    assert.equal(t.envRead("SECRET_TOKEN"), "hunter2-super-secret");
    t.close();
    const raw = readFileSync(path, "utf8");
    assert.ok(!raw.includes("hunter2-super-secret"));
    const span = readLines(path)[1];
    assert.deepEqual(span.output, {
      digest: digestHex("hunter2-super-secret"),
      len: "hunter2-super-secret".length,
    });
  });
});

test("errors are recorded then re-thrown", () => {
  withDir((dir) => {
    const path = join(dir, "t.jsonl");
    const t = new Tracer({ task: "toy", path });
    assert.throws(
      () =>
        t.toolCall("explode", {}, () => {
          throw new RangeError("nope");
        }),
      RangeError,
    );
    t.close();
    const span = readLines(path)[1];
    assert.equal(span.error, "RangeError: nope");
    assert.equal(span.output, null);
  });
});

test("NaN is rejected, not recorded", () => {
  withDir((dir) => {
    const t = new Tracer({ task: "toy", path: join(dir, "t.jsonl") });
    assert.throws(() => t.toolCall("bad", { x: Number.NaN }, () => 1), /NaN/);
    t.close();
  });
});

test("record mode requires a path", () => {
  const saved = process.env.AUTO_TRACE_FILE;
  delete process.env.AUTO_TRACE_FILE;
  try {
    assert.throws(() => new Tracer({ task: "toy" }), /needs a trace path/);
  } finally {
    if (saved !== undefined) process.env.AUTO_TRACE_FILE = saved;
  }
});

test("async leaf calls record at settle", async () => {
  await withDir(async (dir) => {
    const path = join(dir, "t.jsonl");
    const t = new Tracer({ task: "toy", path });
    const out = await t.span("step", async () => {
      return await t.toolCall("fetch", { url: "x" }, async () => "body");
    });
    t.close();
    assert.equal(out, "body");
    const spans = readLines(path).slice(1);
    const fetchSpan = spans.find((s) => s.kind === "tool_call");
    assert.equal(fetchSpan.output, "body");
    assert.equal(fetchSpan.parent_span_id, spans.find((s) => s.kind === "span").span_id);
  });
});

test("replay substitutes recorded outputs without executing", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    recordSample(original);
    const t = new Tracer({ task: "toy", replay: original });
    t.span("step", () => {
      const out = t.toolCall("add", { a: 1, b: 2 }, () => {
        throw new Error("replay must not execute the live call");
      });
      assert.equal(out, 3);
      assert.equal(t.envRead("SAMPLE_VAR"), "hello");
    });
    assert.equal(t.branch("size", { n: 3 }, "small"), "small");
    t.memoryOp("write", "k", () => null, "v");
    assert.equal(t.replayRemaining, 0);
    t.close();
  });
});

test("replay can record its own trace with substituted outputs", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    recordSample(original);
    const replayOut = join(dir, "replay.jsonl");
    const t = new Tracer({ task: "toy", replay: original, path: replayOut });
    t.span("step", () => {
      t.toolCall("add", { a: 1, b: 2 }, () => 99); // substituted: 3
      t.envRead("SAMPLE_VAR");
    });
    t.branch("size", { n: 3 }, "small");
    t.memoryOp("write", "k", () => null, "v");
    t.close();
    const add = readLines(replayOut).find((s) => s.kind === "tool_call");
    assert.equal(add.output, 3);
  });
});

test("replay divergence on different call / input / decision / exhaustion", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    recordSample(original);

    let t = new Tracer({ task: "toy", replay: original });
    assert.throws(() => t.modelCall("add", { a: 1, b: 2 }, () => 3), ReplayDivergence);

    t = new Tracer({ task: "toy", replay: original });
    assert.throws(() => t.toolCall("add", { a: 1, b: 999 }, () => 3), ReplayDivergence);

    t = new Tracer({ task: "toy", replay: original });
    t.toolCall("add", { a: 1, b: 2 });
    t.envRead("SAMPLE_VAR");
    assert.throws(() => t.branch("size", { n: 3 }, "huge"), ReplayDivergence);

    t = new Tracer({ task: "toy", replay: original });
    t.toolCall("add", { a: 1, b: 2 });
    t.envRead("SAMPLE_VAR");
    t.branch("size", { n: 3 }, "small");
    t.memoryOp("write", "k", () => null, "v");
    assert.throws(() => t.toolCall("extra", {}), ReplayDivergence);
  });
});

test("replay divergence on changed environment", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    recordSample(original);
    process.env.SAMPLE_VAR = "changed!";
    const t = new Tracer({ task: "toy", replay: original });
    t.toolCall("add", { a: 1, b: 2 });
    assert.throws(() => t.envRead("SAMPLE_VAR"), ReplayDivergence);
  });
});

test("recorded errors replay as ReplayedError", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    const rec = new Tracer({ task: "toy", path: original });
    assert.throws(() =>
      rec.toolCall("db.query", { q: "select 1" }, () => {
        throw new Error("db down");
      }),
    );
    rec.close();

    const t = new Tracer({ task: "toy", replay: original });
    assert.throws(
      () => t.toolCall("db.query", { q: "select 1" }, () => "fine"),
      ReplayedError,
    );
  });
});

test("replay rejects non-trace files", () => {
  withDir((dir) => {
    const bogus = join(dir, "bogus.jsonl");
    writeFileSync(bogus, '{"v":0,"t":"span","kind":"tool_call","seq":1}\n', "utf8");
    assert.throws(() => new Tracer({ task: "toy", replay: bogus }), /no header/);
  });
});

test("task I/O is recorded on the wire", () => {
  withDir((dir) => {
    const path = join(dir, "t.jsonl");
    const t = new Tracer({ task: "toy", path, taskInput: { doc: "d" } });
    t.toolCall("noop", {}, () => null);
    t.setTaskOutput({ summary: "s", words: 2 });
    t.close();
    const lines = readLines(path);
    const header = lines[0];
    assert.deepEqual(header.task_input, { doc: "d" });
    const outputs = lines.filter((l) => l.t === "task_output");
    assert.equal(outputs.length, 1);
    assert.deepEqual(outputs[0].output, { summary: "s", words: 2 });
    assert.equal(outputs[0].trace_id, header.trace_id);
    assert.equal(outputs[0].v, 0);
    assert.equal(typeof outputs[0].recorded_at_ms, "number");
  });
});

test("setTaskOutput twice throws; null/undefined are rejected", () => {
  withDir((dir) => {
    const t = new Tracer({ task: "toy", path: join(dir, "t.jsonl") });
    assert.throws(() => t.setTaskOutput(null), /not recordable/);
    assert.throws(() => t.setTaskOutput(undefined), /not recordable/);
    t.setTaskOutput("first");
    assert.throws(() => t.setTaskOutput("second"), /twice/);
    t.close();
  });
});

test("wire without task I/O is byte-identical to the pre-ADR-0025 format", () => {
  withDir((dir) => {
    const path = join(dir, "t.jsonl");
    const t = new Tracer({ task: "toy", path, nowMs: () => 1000 });
    t.toolCall("add", { a: 1 }, () => 3);
    t.close();
    const raw = readFileSync(path, "utf8").replaceAll(t.traceId, "T".repeat(32));
    const expected =
      '{"attrs":{},"sdk":"auto-sdk-typescript/0.1.0","started_at_ms":1000,' +
      '"t":"trace","task":"toy","trace_id":"' +
      "T".repeat(32) +
      '","v":0}\n' +
      '{"attrs":{},"duration_ms":0,"error":null,"input":{"a":1},' +
      '"kind":"tool_call","name":"add","output":3,"parent_span_id":null,' +
      '"seq":1,"span_id":1,"started_at_ms":1000,"t":"span",' +
      '"trace_id":"' +
      "T".repeat(32) +
      '","v":0}\n';
    assert.equal(raw, expected);
  });
});

test("replay ignores task_output lines (task I/O plays no role in matching)", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    const rec = new Tracer({ task: "toy", path: original, taskInput: "in" });
    rec.toolCall("add", { a: 1 }, () => 3);
    rec.setTaskOutput("out");
    rec.close();

    const t = new Tracer({ task: "toy", replay: original });
    assert.equal(
      t.toolCall("add", { a: 1 }, () => 99),
      3,
    );
    assert.equal(t.replayRemaining, 0);
    t.close();
  });
});

const delay = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

// -- concurrency-tolerant replay (ADR-0029) ---------------------------------

test("sequential replay in recorded order re-records byte-identically (pin)", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    const replayed = join(dir, "replay.jsonl");
    process.env.SAMPLE_VAR = "hello";
    const run = (t: Tracer) => {
      t.span("step", () => {
        t.toolCall("add", { a: 1, b: 2 }, () => 3);
        t.envRead("SAMPLE_VAR");
      });
      t.branch("size", { n: 3 }, "small");
      t.memoryOp("write", "k", () => null, "v");
    };
    const rec = new Tracer({ task: "toy", path: original, nowMs: () => 1000 });
    run(rec);
    rec.close();
    const rep = new Tracer({ task: "toy", replay: original, path: replayed, nowMs: () => 1000 });
    run(rep);
    assert.equal(rep.replayRemaining, 0);
    rep.close();
    const normalize = (path: string, id: string) =>
      readFileSync(path, "utf8").replaceAll(id, "T".repeat(32));
    assert.equal(normalize(original, rec.traceId), normalize(replayed, rep.traceId));
  });
});

test("replay matches reordered sequential calls order-independently", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    const rec = new Tracer({ task: "toy", path: original });
    rec.toolCall("work", { i: 0 }, () => "r0");
    rec.toolCall("work", { i: 1 }, () => "r1");
    rec.close();

    const t = new Tracer({ task: "toy", replay: original });
    assert.equal(t.toolCall("work", { i: 1 }, () => "live"), "r1");
    assert.equal(t.toolCall("work", { i: 0 }, () => "live"), "r0");
    assert.equal(t.replayRemaining, 0);
    t.close();
  });
});

test("Promise.all replay: interleaved identical-name calls all match", async () => {
  await withDir(async (dir) => {
    const original = join(dir, "orig.jsonl");
    const rec = new Tracer({ task: "conc", path: original });
    for (const i of [0, 1, 2, 3, 4]) rec.toolCall("work", { i }, () => i * 10);
    rec.close();

    // Reversed delays scramble arrival order vs recorded order; under the
    // old shared cursor the first arrival (i=4) drew recorded slot i=0 and
    // threw. First-unconsumed matching pairs each call with its recording.
    const t = new Tracer({ task: "conc", replay: original });
    const outs = await Promise.all(
      [0, 1, 2, 3, 4].map((i) =>
        t.span(`wrap-${i}`, async () => {
          await delay(2 + (4 - i) * 4);
          return t.toolCall("work", { i }, () => -1);
        }),
      ),
    );
    assert.deepEqual(outs, [0, 10, 20, 30, 40]);
    assert.equal(t.replayRemaining, 0);
    t.close();
  });
});

test("replay divergence on unknown call names it and what remains unconsumed", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    recordSample(original);
    const t = new Tracer({ task: "toy", replay: original });
    assert.throws(
      () => t.toolCall("nonexistent", { z: 1 }, () => 1),
      (err: unknown) => {
        assert.ok(err instanceof ReplayDivergence);
        assert.match(err.message, /no unconsumed recorded call matches live tool_call\("nonexistent"\)/);
        assert.match(err.message, /\{"z":1\}/);
        assert.match(err.message, /recorded tool_call\("add"\)/);
        assert.match(err.message, /recorded env_read\("SAMPLE_VAR"\)/);
        return true;
      },
    );
    t.close();
  });
});

test("replay divergence on same call with unmatched input says input differs", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    recordSample(original);
    const t = new Tracer({ task: "toy", replay: original });
    assert.throws(
      () => t.toolCall("add", { a: 1, b: 999 }, () => 3),
      (err: unknown) => {
        assert.ok(err instanceof ReplayDivergence);
        assert.match(err.message, /input differs from recording/);
        assert.match(err.message, /remain unconsumed/);
        return true;
      },
    );
    t.close();
  });
});

test("divergent duplicates are assigned in recorded order by arrival (caveat pin)", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    const rec = new Tracer({ task: "toy", path: original });
    rec.toolCall("dup", {}, () => "first");
    rec.toolCall("dup", {}, () => "second");
    rec.close();

    // sequential replay: program order = arrival order, so assignment is
    // deterministic here; under concurrency arrival order is a race by
    // construction (which call gets which output is undefined)
    const t = new Tracer({ task: "toy", replay: original });
    assert.equal(t.toolCall("dup", {}, () => "live"), "first");
    assert.equal(t.toolCall("dup", {}, () => "live"), "second");
    assert.equal(t.replayRemaining, 0);
    t.close();
  });
});

test("divergent duplicates under Promise.all: each recording consumed exactly once", async () => {
  await withDir(async (dir) => {
    const original = join(dir, "orig.jsonl");
    const rec = new Tracer({ task: "toy", path: original });
    rec.toolCall("dup", {}, () => "first");
    rec.toolCall("dup", {}, () => "second");
    rec.close();

    const t = new Tracer({ task: "toy", replay: original });
    const outs = await Promise.all(
      [0, 1].map((i) =>
        (async () => {
          await delay(2 + (1 - i) * 4);
          return t.toolCall("dup", {}, () => "live");
        })(),
      ),
    );
    assert.deepEqual([...outs].sort(), ["first", "second"]);
    assert.equal(t.replayRemaining, 0);
    t.close();
  });
});

test("unconsumed recording at exit is silent; replayRemaining reports it (pin)", () => {
  withDir((dir) => {
    const original = join(dir, "orig.jsonl");
    recordSample(original);
    const t = new Tracer({ task: "toy", replay: original });
    t.span("step", () => {
      assert.equal(
        t.toolCall("add", { a: 1, b: 2 }, () => 99),
        3,
      );
    });
    assert.equal(t.replayRemaining, 3);
    t.close(); // throws nothing; the leftover recorded calls were simply unused
  });
});

test("concurrent record: Promise.all spans parent to their own wrapper", async () => {
  await withDir(async (dir) => {
    const path = join(dir, "conc.jsonl");
    const t = new Tracer({ task: "conc", path });
    // Later-opened spans settle first: under a shared stack every leaf
    // would parent to the last-opened (or last-unsettled) wrapper.
    await Promise.all(
      [0, 1, 2, 3, 4].map((i) =>
        t.span(`wrap-${i}`, async () => {
          await delay(2 + (4 - i) * 4);
          const out = await t.toolCall(`tool-${i}`, { i }, async () => {
            await delay(3);
            return i * 10;
          });
          assert.equal(out, i * 10);
        }),
      ),
    );
    t.close();

    const [header, ...spans] = readLines(path);
    assert.equal(spans.length, 10); // 5 wrappers + 5 leaves
    assert.ok(spans.every((s) => s.t === "span" && s.v === 0 && s.trace_id === header.trace_id));
    const seqs = spans.map((s) => s.seq).sort((a, b) => a - b);
    assert.deepEqual(seqs, Array.from({ length: spans.length }, (_, k) => k + 1));

    const byId = new Map(spans.map((s) => [s.span_id, s]));
    for (let i = 0; i < 5; i++) {
      const child = spans.find((s) => s.kind === "tool_call" && s.name === `tool-${i}`);
      assert.ok(child, `tool-${i} recorded`);
      const wrapper = byId.get(child.parent_span_id);
      assert.equal(
        wrapper?.name,
        `wrap-${i}`,
        "each leaf parents to its own wrapper, not the last-opened one",
      );
      assert.ok(wrapper.seq < child.seq);
      assert.equal(wrapper.parent_span_id, null);
    }
  });
});

test("concurrent record: nested async spans chain within their own branch", async () => {
  await withDir(async (dir) => {
    const path = join(dir, "nest.jsonl");
    const t = new Tracer({ task: "nest", path });
    await Promise.all(
      [0, 1].map((i) =>
        t.span(`outer-${i}`, async () => {
          await delay(2 + i * 3);
          await t.span(`inner-${i}`, async () => {
            await delay(2 + (1 - i) * 3);
            await t.toolCall(`leaf-${i}`, {}, async () => {
              await delay(2);
              return null;
            });
          });
        }),
      ),
    );
    t.close();

    const spans = readLines(path).slice(1);
    assert.equal(spans.length, 6);
    const byName = new Map(spans.map((s) => [s.name, s]));
    for (const i of [0, 1]) {
      const outer = byName.get(`outer-${i}`);
      const inner = byName.get(`inner-${i}`);
      const leaf = byName.get(`leaf-${i}`);
      assert.equal(outer.parent_span_id, null);
      assert.equal(inner.parent_span_id, outer.span_id);
      assert.equal(leaf.parent_span_id, inner.span_id);
    }
  });
});
