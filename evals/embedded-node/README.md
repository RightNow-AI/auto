# embedded-node — in-process `auto_node` latency bench

The napi twin (ADR-0026) of the embedded-python rung (ADR-0024): the same
last rung of the economics ladder, for agents written in Node/TypeScript.
Earlier rungs measured the cost of a compiled call across process/transport
boundaries:

| rung | mechanism | measured |
| --- | --- | --- |
| tier-0 | frontier interpreter | ~737 ms/call |
| `auto serve` | HTTP, module cached | ~21 ms/call |
| `auto run --stdio` | resident stdio, one pipe hop/call | ~0.29 ms/call |
| `auto_py` | in-process (pyo3), direct call | p50 54.1 us/call (wave-9) |
| **`auto_node`** | **in-process (napi), direct call** | **measured by `bench.js`** |

`auto_node` (crate `crates/auto-node`) is a napi addon that holds the compiled
`.cbin` inside the agent's OWN Node process: the wasm module is compiled once
when you construct `new Runner(path)`, and each `.answer` call runs a fresh
wasm instance reached by a direct function call — no subprocess, no HTTP, no
stdio. `bench.js` measures the per-call latency that remains once the
transport is gone.

## build (operator-run; CI proves it in the optional embedded workflow)

The addon is a cdylib; the cargo gates never build it (the `node` feature is
off for check/clippy/test). Two routes:

**Plain cargo (no extra toolchain).** A `.node` file is the platform dylib
under another name, and `bench.js` loads via `process.dlopen`, which does not
care about the extension — so the raw cargo cdylib works as-is. From the repo
root:

```
cargo build -p auto-node --release --features node
node evals/embedded-node/bench.js path/to/task.cbin inputs.jsonl
```

`bench.js` finds `target/release/auto_node.dll` (`libauto_node.so` /
`libauto_node.dylib`) on its own; copy it to
`crates/auto-node/auto_node.node` or pass `--addon PATH` if you keep it
elsewhere.

**@napi-rs/cli via the npm scaffold (wave-13 packaging).** `crates/auto-node`
ships a `package.json` (private v0 — never published) whose `build` script
drives @napi-rs/cli 3 through npx, so strangers need only node + npx (no
`npm install` step, no lockfile):

```
cd crates/auto-node
npm run build
# = npx --yes --package @napi-rs/cli@3 napi build --release --features node \
#       --dts ../../target/auto_node.generated.d.ts
```

This emits `crates/auto-node/auto_node.node` — the exact spot `bench.js`
looks first, on every platform (the CLI names the copy from the `napi`
config's `binaryName`; no dlopen incantation, `require()` loads it). The CLI
also generates a typedef from the macros; the script redirects it to
`target/auto_node.generated.d.ts` (gitignored) so it never clobbers the
checked-in, HAND-WRITTEN `crates/auto-node/index.d.ts` — hand-written
because the generated file cannot express the error contract (the thrown
`code`s and the abstention properties), which is half the API.

Labeled honestly: BOTH routes are now measured on this repo (Windows 11, rustc 1.96.1, node v22.20.0, @napi-rs/cli 3.7.2). `npm run
build` exits 0, emits `auto_node.node` (17,690,112 bytes), the addon
`require()`-loads, `version()` answers `0.0.1`, a missing artifact refuses
with `code === "AutoError"`, and the guarded fixture trips with the
`reason`/`distance`/`threshold` properties — the same smoke the cargo route
passed. The echo-pure bench through the npm-built addon: p50 19.000 us,
outcomes 500/0/0 (500 iters, consistent with the first measured run below).

## run

`INPUTS.jsonl` is one RAW JSON value per line — the same protocol `auto run
--stdio` reads. Any pure `.cbin` compiled by `auto compile` works:

```
node evals/embedded-node/bench.js path/to/task.cbin path/to/inputs.jsonl
node evals/embedded-node/bench.js task.cbin inputs.jsonl --warmup 500 --iters 20000 --addon crates/auto-node/auto_node.node
```

No compiled task at hand? A labeled fixture generator (an `#[ignore]`d test,
never run by the gates) writes the canonical runnable echo fixtures — real
wasmtime-compiled artifacts, pure and guarded — plus matching inputs under
`target/embedded-node-bench/`:

```
cargo test -p auto-node --test twin_contract -- --ignored write_bench_fixture
node evals/embedded-node/bench.js target/embedded-node-bench/echo-pure.cbin target/embedded-node-bench/inputs.jsonl
```

### first measured run (one machine's wall time, not a guarantee)

Windows 11, node v22.20.0, release addon, echo-pure fixture,
defaults (200 warmup / 5000 timed): one-time load 4793.8 us; per-call
p50 18.200 us, p95 24.905 us, mean 19.553 us; 50,267 calls/sec; outcomes
5000/0/0. The guarded fixture over a 50/50 proceed/trip input mix (100/2000):
p50 16.100 us, outcomes output=1000 abstained=1000 error=0 — abstentions are
timed and counted, not hidden. For scale: the stdio rung's floor is
~290 us/call on the same ladder.

Output reports the one-time load separately, then p50/p95/mean microseconds
per call and calls/sec over the timed region. Warmup calls are executed but
not counted. A guarded artifact may abstain on some inputs; abstentions are
timed like any call and counted in the `outcomes` line, not hidden. The
percentile method is linear interpolation between ranks (numpy's "linear"),
same as `bench.py`, so the two twins' numbers are comparable.

## error contract (frozen, ADR-0026)

Every failure is a thrown JS `Error` with a `code` property:

- `code === "AutoError"` — artifact load failure or tier-1 parse/execution
  failure.
- `code === "AutoAbstained"` — the runtime guard tripped; the message carries
  the guard detail and `reason` (string | null), `distance` (number | null —
  null for a wrong-shaped input with no text to measure), and `threshold`
  (number | null) ride as own properties. There is no in-process tier-0: an
  abstention never deopts (the same per-request spend-policy gap as `auto
  serve` and `auto run --stdio`, spec/runtime.md §9).

## limitations (labeled, honest)

- **Pure artifacts only (v0).** A capability-bearing artifact (nonempty
  manifest `capabilities`) is refused at LOAD with the frozen ADR-0024
  message: *"capability artifacts are not supported embedded in v0 (recorded:
  per-request tool policy + host callbacks)"*. The auto-py twin has since
  grown `tools=` host callbacks (ADR-0027); bringing this twin to parity is a
  recorded follow-up in ADR-0026, not a hidden gap.
- **Synchronous on the JS thread.** `.answer` blocks the event loop for the
  duration of the wasm call (microseconds for compiled artifacts — that is
  the point). Unlike the pyo3 twin there is no GIL to release; an async
  surface for long-running artifacts is a recorded follow-up.
- **On CI as an OPTIONAL workflow only.** The workspace correctness gates
  (`cargo check|clippy|test`) still do NOT build the addon (the `node`
  feature is off there; no Node toolchain gates the Rust).
  `.github/workflows/embedded.yml` builds the addon via the npm script and
  runs the `require()` smoke on ubuntu/node 22, on every push to main and
  every PR — deliberately NOT a required check (optional by convention; see
  the workflow's header comment).
- **Private npm scaffold; no publish, no prebuilds.** `package.json` is
  `private: true`; nothing goes to a registry, and there are no prebuilt
  binaries — the addon is built on and FOR the build host (platform = build
  host). The hand-written `crates/auto-node/index.d.ts` documents the frozen
  surface and the error contract for TypeScript callers.
- **Measured wall time, per machine.** The numbers are `process.hrtime.bigint`
  wall time on the machine that ran the bench. This is not a parity claim and
  not a cross-machine guarantee — it is what this rung costs there.
