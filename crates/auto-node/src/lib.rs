//! auto-node — the compiled `.cbin` embedded IN-PROCESS in Node.js
//! (ADR-0026; the napi twin of `crates/auto-py`, ADR-0024).
//!
//! The economics ladder's last rung, for the OTHER agent language. A one-shot
//! `auto run` pays process spawn plus module compilation on every call;
//! `auto run --stdio` amortizes the compile but still pays a line-framed pipe
//! hop per call (wave-6: ~0.29 ms/call); the wave-9 python rung measured the
//! in-process call at p50 54.1 us. The TS SDK's agents deserve the same
//! boundary. This crate holds the compiled artifact inside the agent's OWN
//! Node process: [`bindings`]' `Runner` wraps [`auto_runtime::Runner`] (the
//! wasm module is compiled ONCE at construction; each `.answer` runs a fresh
//! wasm instance — the frozen one-`run`-per-instance ABI, so no cross-call
//! state leaks), reached by a direct call with no subprocess, HTTP, or stdio.
//!
//! v0 embeds PURE artifacts only (mirroring ADR-0024 decision 4): a
//! capability-bearing artifact refuses at LOAD with the frozen ADR-0024
//! message. The tools= host callbacks auto-py grew in ADR-0027 are a recorded
//! follow-up for this twin, not silently absent.
//!
//! JS surface (ADR-0026, frozen):
//! - `new Runner(artifactPath: string)` — read + parse + compile once; any
//!   load failure (unreadable path, bad container/manifest, a capability
//!   artifact, a module that will not compile) throws an `Error` whose
//!   `code` is `"AutoError"`.
//! - `Runner.answer(inputJson: string): string` — one JSON value in, the
//!   tier-1 OUTPUT value out as canonical JSON text. A guard trip throws an
//!   `Error` with `code === "AutoAbstained"` (message = the guard detail;
//!   `reason`/`distance`/`threshold` ride as structured properties, each
//!   `null` when the envelope carried none); any parse/execution failure
//!   throws with `code === "AutoError"`. This mirrors `auto run --stdio`'s
//!   per-line `{"output"}` | `{"abstained"}` | `{"error"}` decision
//!   (spec/runtime.md §9), surfaced as return-value / throw / throw — and
//!   the auto_py twin's return / `AutoAbstained` / `AutoError`. There is no
//!   in-process tier-0: an abstention never deopts.
//! - `version(): string` — this crate's version (`CARGO_PKG_VERSION`).
//!
//! Threading: unlike the pyo3 twin there is no GIL to release — `.answer` is
//! a synchronous call ON the JS thread, so it blocks the event loop for the
//! duration of the wasm run (microseconds for compiled artifacts; that is the
//! point). An async `AsyncTask` surface for long-running artifacts is a
//! recorded follow-up (ADR-0026), not a hidden gap.
//!
//! unsafe: napi/napi-derive's `#[napi]` macros expand to N-API C-ABI glue
//! that is necessarily `unsafe`. The workspace DENIES (not forbids)
//! `unsafe_code` precisely so justified FFI can allow it locally — the same
//! precedent as pyo3 in auto-py and the flatc accessors in auto-ir. The
//! allow is gated on the `node` feature, so the pure-logic build the cargo
//! gates compile keeps the full deny; [`logic`] carries no unsafe and is
//! unit-tested without any napi type or a Node runtime.
#![cfg_attr(feature = "node", allow(unsafe_code))]

// pub (unlike the auto-py twin's private module): with the `node` feature off
// the FFI does not compile, so the pure logic IS this crate's whole rlib
// surface — public keeps it honestly reachable (and lint-visible) for the
// gates and for any host-side harness.
pub mod logic;

// The FFI surface. Only the `node` feature compiles it (the plain cargo gates
// build pure Rust and need no Node toolchain), and even then it is cfg'd OUT
// of the unit-test binary so `cargo test --features node` links no N-API
// symbols (the pure logic in `logic` is what the unit tests cover; the FFI is
// exercised end-to-end by evals/embedded-node/bench.js against a built
// auto_node.node addon).
#[cfg(all(feature = "node", not(test)))]
mod bindings;
