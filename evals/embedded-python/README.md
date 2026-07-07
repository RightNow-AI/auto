# embedded-python — in-process `auto_py` latency bench

The last rung of the economics ladder (ADR-0024). Earlier rungs measured the
cost of a compiled call across process/transport boundaries:

| rung | mechanism | measured |
| --- | --- | --- |
| tier-0 | frontier interpreter | ~737 ms/call |
| `auto serve` | HTTP, module cached | ~21 ms/call |
| `auto run --stdio` | resident stdio, one pipe hop/call | ~0.29 ms/call |
| **`auto_py`** | **in-process, direct call** | **measured by `bench.py`** |

`auto_py` (crate `crates/auto-py`) is a pyo3/abi3 extension that holds the
compiled `.cbin` inside the agent's OWN Python process: the wasm module is
compiled once when you construct `auto_py.Runner(path)`, and each `.answer`
call runs a fresh wasm instance reached by a direct function call — no
subprocess, no HTTP, no stdio. `bench.py` measures the per-call latency that
remains once the transport is gone.

## build (local; CI proves it in the optional embedded workflow)

`maturin` builds the extension. From the repo root, either route:

**Into the active interpreter (development):**

```
pip install maturin
maturin develop --release -m crates/auto-py/Cargo.toml
```

`maturin develop` installs `auto_py` into the current interpreter (use a
virtualenv if you want isolation).

**As an abi3 wheel (the packaged artifact — wave-13):**

```
maturin build --release -m crates/auto-py/Cargo.toml
pip install --no-index --no-deps --force-reinstall target/wheels/auto_py-*.whl
```

Measured on this repo (Windows 11, rustc 1.96.1, maturin
1.14.1): `maturin build` reports "Built wheel for abi3 Python ≥ 3.10" and
emits `target/wheels/auto_py-0.0.1-cp310-abi3-win_amd64.whl`
(6,545,822 bytes — the platform tag is the BUILD host's; one wheel per
platform, no cross-platform matrix in v0), and the offline pip install +
`import auto_py` / `auto_py.version()` smoke passes. The same build + smoke
runs on ubuntu/CPython 3.10 (the abi3 floor) in
`.github/workflows/embedded.yml`. Private v0: the wheel is never published
to PyPI.

## run

`INPUTS.jsonl` is one JSON value per line — the same protocol `auto run
--stdio` reads. Any `.cbin` compiled by `auto compile` works, e.g. a pure
artifact from `evals/`:

```
python evals/embedded-python/bench.py path/to/task.cbin path/to/inputs.jsonl
python evals/embedded-python/bench.py path/to/task.cbin inputs.jsonl --warmup 500 --iters 20000
```

Output reports the one-time load separately, then p50/p95/mean microseconds per
call and calls/sec over the timed region. Warmup calls are executed but not
counted. A guarded artifact may abstain on some inputs; abstentions are timed
like any call and counted in the `outcomes` line, not hidden.

## limitations (labeled, honest)

- **Pure artifacts only (v0).** A capability-bearing artifact (nonempty
  manifest `capabilities`) is refused at LOAD: `auto_py.Runner(path)` raises
  `AutoError` with *"capability artifacts are not supported embedded in v0
  (recorded: per-request tool policy + host callbacks)"*. Embedding a tool host
  needs a per-request tool policy and host callbacks — a recorded follow-up,
  not this slice.
- **abi3-py310+.** The extension targets the CPython stable ABI from 3.10
  onward; it will not import on 3.9 or earlier.
- **On CI as an OPTIONAL workflow only.** The workspace correctness gates
  (`cargo check|clippy|test`) still do NOT build the extension (the
  `extension-module` feature is off there, so no CPython is needed to gate
  the Rust). `.github/workflows/embedded.yml` builds the wheel on
  ubuntu/CPython 3.10 and smoke-imports the installed wheel, on every push
  to main and every PR — deliberately NOT a required check (optional by
  convention; see the workflow's header comment).
- **Private v0; no PyPI, no wheel matrix.** The wheel is never published;
  platform = build host.
- **Measured wall time, per machine.** The numbers are `perf_counter_ns` wall
  time on the machine that ran the bench. This is not a parity claim and not a
  cross-machine guarantee — it is what this rung costs here.
- **No in-process tier-0.** A guard trip abstains (raises `AutoAbstained`); it
  never deopts to a frontier model, exactly as `auto serve` and `auto run
  --stdio` (the same per-request spend-policy gap, spec/runtime.md §9).
- **GIL released around the call.** `.answer` releases the GIL
  (`Python::detach`) around the wasm execution, so a long call does not freeze
  the host agent's other Python threads.
