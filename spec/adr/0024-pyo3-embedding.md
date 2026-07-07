# ADR-0024: `auto-py` — the compiled artifact embedded in-process in Python

status: accepted · scope: `crates/auto-py` (new crate), workspace `Cargo.toml` (one member), `evals/embedded-python/`

## context

The economics ladder measures what a compiled call costs once each layer of
overhead is removed:

| rung | mechanism | measured |
| --- | --- | --- |
| tier-0 | frontier interpreter | ~737 ms/call |
| `auto serve` | HTTP, module cached (ADR-0011) | ~21 ms/call |
| `auto run --stdio` | resident stdio runner, one pipe hop/call (wave-6) | ~0.29 ms/call |
| in-process | direct call, no transport | this ADR's target |

The wave-5 systems finding was that glue, HTTP framing, and wasm instantiation
— not the compiled logic — dominate the compiled path; wave-6's resident stdio
runner (`Runner` in `crates/auto-runtime/src/runner.rs`) drove that to
~0.29 ms/call by compiling the module once and answering a line protocol. The
remaining overhead is the process boundary itself: an agent written in Python
still spawns/pipes to a separate `auto` process. The recorded follow-up ("pyo3
bindings later") closes it: put the compiled artifact inside the agent's OWN
process. `auto-py` is a pyo3 extension whose `Runner` wraps
`auto_runtime::Runner` unchanged — module compiled once at construction, a
fresh wasm instance per `.answer` (the frozen one-`run`-per-instance ABI, so no
cross-call state leaks) — reached by a direct function call. The orchestrator
builds it with maturin and measures per-call latency against the 0.29 ms stdio
floor (`evals/embedded-python/bench.py`).

## decision

1. **pyo3 0.29, pinned `0.29` (caret).** Current stable (2026-06-11), MSRV
   1.83 — comfortably under the workspace's rust 1.96. Caret `0.29` accepts
   non-breaking 0.29.x patches but excludes 0.30, which can change the
   proc-macro surface. pyo3 0.29 renamed the GIL-release primitive
   `Python::allow_threads` to `Python::detach` (same `Ungil` bound); we use
   `detach`.

2. **abi3 (`abi3-py310`), not version-locked.** The extension is built against
   CPython's stable ABI from 3.10 onward, so ONE artifact serves 3.10+ and
   `cargo check`/`clippy` resolve the ABI from the feature alone without
   pinning a single interpreter build. Cost: the abi3 subset (no
   version-specific fast paths); irrelevant here, where the hot work is inside
   wasmtime, not the C-API boundary.

3. **`extension-module` is a crate feature, OFF for `cargo` dev commands, ON
   only for the wheel.** `pyo3/extension-module` tells pyo3 not to link
   libpython (the symbols come from the host interpreter at load). Enabling it
   unconditionally makes `cargo test` unable to link a test harness on Unix.
   So `crates/auto-py` declares `[features] extension-module =
   ["pyo3/extension-module"]` and `pyproject.toml` turns it on for
   `maturin build`. `cargo check|clippy|test` run without it. Combined with
   decision 7, none of the three Rust gates needs CPython at link time.

4. **Pure artifacts only in v0; refuse capability artifacts at LOAD.** Tools
   are out of scope for the first embedded slice. `Runner.__new__` parses the
   manifest and, if `capabilities` is nonempty, raises `AutoError` with the
   frozen message *"capability artifacts are not supported embedded in v0
   (recorded: per-request tool policy + host callbacks)"* — a loud refusal at
   LOAD, not a surprise at call time. (`auto_runtime::Runner::new` would itself
   refuse a capability artifact through the loader, but with a generic
   "no tool host provided" message; we refuse first, with the honest reason.)
   The refusal predicate is a pure function over the parsed capability list,
   unit-tested without pyo3.

5. **Exceptions, not sentinel returns.** `.answer(input_json: str) -> str`
   returns the tier-1 OUTPUT value as canonical JSON text. The other two
   outcomes of the runner's `{"output"} | {"abstained"} | {"error"}` decision
   (spec/runtime.md §9) become exceptions: a guard trip raises `AutoAbstained`
   (message = the guard detail: reason, distance, threshold) and any
   parse/execution failure raises `AutoError`. Python callers use `try/except`;
   no in-band sentinel to misread. The envelope→outcome decode is a pure
   function, unit-tested against the exact shapes `runner.rs` emits.

6. **Release the GIL around the wasm call.** `.answer` copies the input to an
   owned `String` (so nothing borrowed from Python is read GIL-free), then runs
   the wasm call inside `Python::detach`. A long inference therefore does not
   freeze the host agent's other Python threads. `auto_runtime::Runner` is
   `Send + Sync`, and the pyclass is `frozen` (read-only, `&self`-only), so the
   shared borrow held across the release is sound. `detach`'s `Ungil` bound
   statically rejects smuggling a Python handle into the released region.

7. **Pure logic in `logic.rs`; FFI in `bindings.rs`, cfg'd out of the test
   binary.** The two pure functions (capability refusal, envelope decode) carry
   no pyo3 type and are the unit-test surface. `bindings.rs` (the module,
   class, exceptions) is `#[cfg(not(test))]`, so `cargo test` builds a harness
   that references no CPython symbols and needs no interpreter at run time,
   while `cargo check`/`clippy --all-targets` still compile and lint the FFI
   via the lib target. The FFI is exercised end-to-end by maturin + `bench.py`.
   `[lib] crate-type = ["cdylib", "rlib"]`: cdylib is the extension; rlib lets
   the test/clippy lib target build normally.

8. **unsafe: a justified crate-level allow.** pyo3's macros expand to CPython
   C-API glue that is necessarily `unsafe`. The workspace DENIES (not forbids)
   `unsafe_code` precisely so justified FFI can `#![allow(unsafe_code)]`
   locally — the same precedent as the flatc accessors in `auto-ir`. `logic.rs`
   contains no unsafe.

## alternatives considered

**`ctypes`/`cffi` over a plain `cdylib` C ABI.** Export `extern "C"` functions
and call them from Python with `ctypes`. No pyo3 dependency. Rejected: we would
hand-roll string marshalling, error signalling, and GIL handling that pyo3
gives correctly and safely; exceptions (decision 5) and GIL release
(decision 6) would become manual and error-prone. pyo3's cost is one build-time
dependency, paid once.

**napi (Node) first.** The agent ecosystem is also heavily TypeScript, and the
recorded SDK list names a TS shim. Rejected as the *first* twin only because
the resident runner and the eval harness are already Python-shaped and the
Python bench compares directly against the stdio floor. A napi twin over the
same `auto_runtime::Runner` seam is a recorded follow-up.

**Keep stdio; do not embed.** The 0.29 ms stdio runner already amortizes
compilation. Rejected as the end state: it still pays a process boundary and a
line-framed pipe hop per call, and an in-process agent should not shell out to
itself. Embedding removes the last transport; stdio remains for language-
agnostic and process-isolation cases.

**Sentinel return values instead of exceptions** (e.g. return the raw
`{"abstained": ...}` object). Rejected: it pushes envelope parsing onto every
caller and invites treating an abstention as an answer — the silent-correctness
failure guards exist to prevent. Exceptions make abstention impossible to
ignore.

**Embed CPython in a Rust host (pyembed/PyOxidizer)** instead of building an
extension for the caller's interpreter. Rejected: inverts control — the agent
already IS the Python process; we want to live inside it, not host our own.

## consequences

- A Python agent can hold a compiled `.cbin` resident and answer on the tier-1
  fast path with a direct call — no subprocess, HTTP, or stdio. The per-call
  latency below the stdio floor is measured by `bench.py` (orchestrator-run;
  this ADR does not fabricate a number).
- The three Rust gates (`cargo check|clippy|test -p auto-py`) pass without a
  Python interpreter at link time (measured on rust 1.96.1). Building the wheel
  needs CPython 3.10+ and maturin.
- v0 embeds pure artifacts only; capability artifacts are refused loudly at
  load. Tool support, wheels/CI, a napi twin, and structured exception
  attributes are recorded follow-ups (below), not hidden gaps.

## recorded follow-ups

- **Embedded tool host.** A `HostTools`-bearing constructor
  (`Runner.with_tools(...)`) mapping declared capabilities to Python callables
  — the per-request tool policy + host callbacks the v0 refusal names. Needs an
  authorization/accounting model (the same gap as ADR-0011 decision 2).
- **Wheels + CI.** Build abi3 wheels per platform and add the optional CI job
  in the appendix; publish to the registry alongside `.cbin`s.
- **napi twin.** The same seam exposed to Node/TypeScript.
- **Structured exception attributes.** Expose `reason`/`distance`/`threshold`
  as attributes on `AutoAbstained` in addition to the message string.

## sources

- pyo3 0.29.0 (crates.io: published 2026-06-11, `rust_version = 1.83`).
  `Python::detach<T, F>(self, f: F) -> T where F: Ungil + FnOnce() -> T, T:
  Ungil` — `pyo3-0.29.0/src/marker.rs:561`; `allow_threads` is gone in 0.29.
  abi3 / `extension-module` semantics: <https://pyo3.rs/v0.29.0/building-and-distribution.html>
- maturin (build backend): `maturin develop`/`build`, `[tool.maturin] features`
  — <https://www.maturin.rs/>
- workspace toolchain: `rustc 1.96.1 (2026-06-26)`; workspace `rust-version =
  1.96`, `edition = 2024`.

## appendix: proposed optional CI step (not added by this change)

**LANDED (wave-13)** as the `embedded-python` job of
`.github/workflows/embedded.yml` — a SEPARATE workflow (ci.yml untouched),
optional by convention: not a required check, but the job itself is not
`continue-on-error`. What shipped differs from the sketch below (kept for
history): ubuntu-only, CPython 3.10 = the abi3 floor (no 3-OS matrix — no
wheel matrix in v0, platform = build host); `checkout@v5` /
`setup-python@v6` per ci.yml conventions; the pinned-flatc install and
rust-cache steps the sketch omitted (auto-ir sits in the dep tree via
auto-backend/auto-runtime and its build.rs needs flatc, ADR-0001); and the
smoke pip-installs the BUILT WHEEL (`--no-index --no-deps`) and imports
that — proving the artifact itself rather than a second `maturin develop`
build. Measured on the dev host the same day:
`target/wheels/auto_py-0.0.1-cp310-abi3-win_amd64.whl` (6,545,822 bytes,
maturin 1.14.1), install + import smoke green. Still open from the
follow-up: wheels for platforms beyond the CI/dev hosts, and registry
publishing.

The original proposal (Rust gates already run in the main matrix without
CPython):

```yaml
  embedded-python:
    name: auto-py wheel smoke (optional)
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, windows-latest, macos-latest]
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: "3.10"
      - uses: dtolnay/rust-toolchain@1.96.1
      - run: pip install maturin
      - run: maturin build --release -m crates/auto-py/Cargo.toml
      # smoke: build the extension, import it, and assert version() answers.
      - run: maturin develop --release -m crates/auto-py/Cargo.toml
      - run: python -c "import auto_py; assert auto_py.version()"
```

Kept optional because it is the only job needing a Python toolchain; the Rust
correctness gates do not.
