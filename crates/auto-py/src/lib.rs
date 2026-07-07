//! auto-py — the compiled `.cbin` embedded IN-PROCESS in Python (ADR-0024;
//! tools per ADR-0027).
//!
//! The economics ladder's last rung. A one-shot `auto run` pays process spawn
//! plus module compilation on every call; `auto run --stdio` amortizes the
//! compile but still pays a line-framed pipe hop per call (wave-6:
//! ~0.29ms/call). This crate holds the compiled artifact inside the agent's
//! OWN Python process: [`bindings::Runner`] wraps [`auto_runtime::Runner`] (the
//! wasm module is compiled ONCE at construction; each `.answer` runs a fresh
//! wasm instance — the frozen one-`run`-per-instance ABI, so no cross-call
//! state leaks), reached by a direct call with no subprocess, HTTP, or stdio.
//!
//! Capability artifacts run embedded through HOST TOOL CALLBACKS (ADR-0027,
//! closing ADR-0024's recorded follow-up): `tools={name: callable}` maps each
//! declared capability to a Python callable, loaded through the same
//! `HostTools` loader that guards every other surface — the dict keys must
//! cover the declared capabilities EXACTLY (missing, extra, or any tools on a
//! pure artifact refuse at LOAD, naming the offender). Without `tools`, pure
//! artifacts load unchanged and capability artifacts refuse at load.
//!
//! Python surface (ADR-0024, extended by ADR-0027 and ADR-0032):
//! - `Runner(artifact_path: str, tools: dict[str, callable] | None = None,
//!   max_tool_calls: int | None = None)` — read + parse + compile once;
//!   raises `AutoError` on any load failure (unreadable path, bad
//!   container/manifest, a tools/capabilities mismatch, a module that will
//!   not compile, or a `max_tool_calls` on a pure artifact — a tool budget
//!   needs `tools=`, nothing to bound).
//! - `max_tool_calls=N` (ADR-0032 — ADR-0028's serve budget, embedded) caps
//!   ONE `.answer` at `N` EXECUTED tool-callable invocations: the counter
//!   resets at the top of every `.answer`; the `N+1`-th call within one
//!   answer is refused without invoking the callable and the artifact traps
//!   (`AutoError`: `tool budget exceeded: N per answer (ADR-0032)`). Each
//!   executed call logs one stderr audit line
//!   `tool audit: <name> call #<k> (embedded)`; refused attempts are not
//!   audit lines — the breach is the trap. `None` = unlimited, byte-identical
//!   to the pre-budget behavior, no audit. The counter is per Runner, so the
//!   budget is exact only while `.answer` calls on one `Runner` do not
//!   overlap (stated, ADR-0032).
//! - `Runner.answer(input_json: str) -> str` — one JSON value in, the tier-1
//!   OUTPUT value out as canonical JSON text. A guard trip raises
//!   `AutoAbstained` (message = the guard detail; `reason`/`distance`/
//!   `threshold` ride as attributes); any parse/execution failure raises
//!   `AutoError`. This mirrors `auto run --stdio`'s per-line `{"output"}` |
//!   `{"abstained"}` | `{"error"}` decision (spec/runtime.md §9), surfaced as
//!   return-value / exception / exception.
//! - tool callables are str -> str: canonical input JSON in, output JSON text
//!   out. A raising callable (or a non-str / non-JSON return) becomes the
//!   tool's `{"err"}` envelope — an honest trap, never a host crash.
//! - `version() -> str` — this crate's version (`CARGO_PKG_VERSION`).
//!
//! GIL: `.answer` releases the GIL (`Python::detach`, pyo3 0.29's renamed
//! `allow_threads`) around the wasm call so a long inference never freezes the
//! host agent's other threads; the input is copied to an owned `String` first,
//! so nothing borrowed from Python is read GIL-free. A tool callback
//! RE-ATTACHES (`Python::attach`) for exactly the duration of its Python call
//! and detaches again after. `auto_runtime::Runner` is `Send + Sync` (the
//! pyclass is `frozen`), so the shared borrow held across the release is
//! sound. Tool calls serialize on the host's mutex; a callable must not
//! re-enter the same `Runner` (documented deadlock, ADR-0027).
//!
//! unsafe: pyo3's `#[pymodule]` / `#[pyclass]` / `#[pymethods]` /
//! `create_exception!` macros expand to CPython C-API glue that is necessarily
//! `unsafe`. The workspace DENIES (not forbids) `unsafe_code` precisely so
//! justified FFI can allow it locally — the same precedent as the flatc
//! accessors in auto-ir. The pure logic in [`logic`] carries no unsafe and is
//! unit-tested without any pyo3 type or a Python interpreter.
#![allow(unsafe_code)]

mod logic;

// The FFI surface. Compiled for the cdylib and for `cargo check`/`clippy`'s
// lib target, but cfg'd OUT of the unit-test binary so `cargo test` links no
// CPython (the pure logic in `logic` is what the unit tests cover; the FFI is
// exercised end-to-end by evals/embedded-python/bench.py under maturin).
#[cfg(not(test))]
mod bindings;
