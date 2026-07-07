# ADR-0026: `auto-node` — the compiled artifact embedded in-process in Node.js

status: accepted · scope: `crates/auto-node` (new crate), workspace `Cargo.toml` (one member), `evals/embedded-node/`

## context

The economics ladder measures what a compiled call costs once each layer of
overhead is removed:

| rung | mechanism | measured |
| --- | --- | --- |
| tier-0 | frontier interpreter | ~737 ms/call |
| `auto serve` | HTTP, module cached (ADR-0011) | ~21 ms/call |
| `auto run --stdio` | resident stdio runner, one pipe hop/call (wave-6) | ~0.29 ms/call |
| `auto_py` | in-process pyo3 (ADR-0024, wave-9) | p50 54.1 us/call |
| in-process Node | direct call, no transport | this ADR's target |

ADR-0024 closed the process boundary for Python agents and recorded "napi
twin" as a follow-up: the agent ecosystem is heavily TypeScript too (the
trace SDK already ships `sdk/typescript`), and a Node agent that wants tier-1
today still spawns/pipes to a separate `auto` process and pays the ~0.29 ms
stdio floor per call. `auto-node` closes that follow-up: a napi addon whose
`Runner` wraps `auto_runtime::Runner` unchanged — module compiled once at
construction, a fresh wasm instance per `.answer` (the frozen
one-`run`-per-instance ABI, so no cross-call state leaks) — reached by a
direct function call. The operator builds the addon and measures per-call
latency against the stdio floor and the pyo3 twin
(`evals/embedded-node/bench.js`, the mirror of `bench.py`).

## decision

1. **napi 3, pinned EXACT: `napi =3.10.3`, `napi-derive =3.5.9`,
   `napi-build =2.3.2`.** napi 3 is the current stable major (registry
   checked; napi 2 is maintenance); MSRV 1.88 — under the
   workspace's rust 1.96. Exact pins (not caret, unlike auto-py's pyo3
   `0.29`): the proc-macro output, the runtime crate, and the module
   registration glue must agree on one version, and this ADR cites specific
   napi source lines as the basis for the error mechanics (decision 4) — a
   silent patch bump would invalidate the citations. Same exact-pin precedent
   as flatbuffers (ADR-0001). Bumps are deliberate, with the citations
   re-verified.

2. **The FFI is a crate feature (`node`), OFF for the cargo gates.**
   `cargo check|clippy|test -p auto-node` compile pure Rust: `napi` /
   `napi-derive` are optional deps behind `[features] node`, `build.rs` calls
   `napi_build::setup()` only when `CARGO_FEATURE_NODE` is set, and
   `bindings.rs` is `#[cfg(all(feature = "node", not(test)))]` — so no gate
   needs a Node toolchain at build or link time (the mirror of ADR-0024
   decisions 3 and 7). `[lib] crate-type = ["cdylib", "rlib"]`: cdylib is the
   addon; rlib lets the test/clippy lib target build normally. The addon
   itself is built by the operator with `--features node`
   (evals/embedded-node/README.md).

3. **Frozen JS surface; exceptions, not sentinel returns.**
   `new Runner(artifactPath: string)`;
   `Runner.answer(inputJson: string): string` returning the tier-1 OUTPUT
   value as canonical JSON text; module-level `version(): string`. The other
   two outcomes of the runner's `{"output"} | {"abstained"} | {"error"}`
   decision (spec/runtime.md §9) are thrown errors distinguished by a `code`
   property — `"AutoAbstained"` for a guard trip (message = the guard detail;
   `reason: string | null`, `distance: number | null`,
   `threshold: number | null` as own properties), `"AutoError"` for any
   load/parse/execution failure. `code` (not `instanceof`) because a Node
   addon cannot export a subclass identity across contexts as cheaply and
   idiomatically as CPython exports exception types — `code` is the Node
   convention (`err.code === 'ENOENT'`) and survives serialization. Sentinel
   returns are rejected for ADR-0024 decision 5's reason: an abstention must
   be impossible to mistake for an answer.

4. **Error mechanics, verified against the pinned napi source.** Two napi
   facts (napi-3.10.3 `src/error.rs`) carry the contract: (a) `Error<S>` is
   generic over any `AsRef<str>` status, and the throw path passes the status
   string to `napi_create_error` as the JS error's `code` — so a custom
   status enum (`AutoError` / `AutoAbstained`) yields the right `code` with
   no hand-rolled glue; the derive's `Err` arm is
   `JsError::from(err).throw_into(env)`, generic over the status type
   (napi-derive-backend-5.1.1 `src/codegen/fn.rs`). (b) `Error::from(Unknown)`
   holds a `napi_ref` to the original JS object, and `into_value` REUSES the
   referenced object when thrown ("keeps its subclass, stack, and own
   properties"). So `answer` builds the abstention error object once, sets
   `reason`/`distance`/`threshold` (`Option::None` converts to JS `null`),
   wraps it in `Error::from(unknown)`, and returns `Err` — napi throws that
   exact object. No manual `napi_throw`, no pending-exception bookkeeping.

5. **Pure artifacts only in v0; refuse capability artifacts at LOAD.** The
   constructor parses the manifest and, if `capabilities` is nonempty, throws
   `code === "AutoError"` with the frozen ADR-0024 message: *"capability
   artifacts are not supported embedded in v0 (recorded: per-request tool
   policy + host callbacks)"* — a loud refusal at LOAD, not a surprise at
   call time (`auto_runtime::Runner::new` would refuse through the loader,
   but with a generic missing-tools message; we refuse first, with the honest
   reason). The auto-py twin has since closed its refusal with `tools=` host
   callbacks (ADR-0027); parity for this twin is a recorded follow-up, not
   part of this slice. The refusal predicate is a pure function over the
   parsed capability list, unit-tested without napi.

6. **Synchronous on the JS thread.** `.answer` is a direct call that blocks
   the event loop for the duration of the wasm run — microseconds for
   compiled artifacts, which is the point of the rung. There is no GIL
   analogue to release (the pyo3 twin's decision 6 has no counterpart); a
   long-running artifact would starve the loop, so an `AsyncTask` surface is
   a recorded follow-up rather than silently absent.

7. **Pure logic in `logic.rs`; FFI in `bindings.rs`; a deliberate, labeled
   duplication of the auto-py logic.** The refusal predicate and the
   envelope→outcome decode are copied from `crates/auto-py/src/logic.rs`
   (comment-linked both ways in source, minus the `tools=` table this twin
   does not have) rather than shared through a crate: depending on `auto-py`
   would drag pyo3 into this crate's build graph, and extracting a third
   `auto-embed-core` crate for ~150 lines is premature at two twins — record
   it as the refactor trigger when a third embedding (or `tools=` parity)
   lands. Both twins' unit tests pin the same envelope shapes from
   `runner.rs`, which is what actually keeps them aligned. The unsafe allow
   is `#![cfg_attr(feature = "node", allow(unsafe_code))]` with the pyo3/
   flatc precedent — and the gates' feature-off build keeps the full workspace
   deny.

## alternatives considered

**Run the wasm in Node's own engine (wasm-in-node).** The `.cbin`'s code IS a
wasm module; V8 has a wasm runtime; a pure-JS loader would need no native
build at all — the strongest alternative, and the recorded follow-up most
worth doing. Rejected for v0 because the artifact is not just the module:
container + manifest parsing and cross-checks, the loader's refusal rules
(capabilities, unexpected imports), guard evaluation (embedding distance +
conformal threshold), and canonical JSON all live in `auto-runtime` /
`auto-backend` Rust. A JS reimplementation is a SECOND enforcement point that
can drift silently — and a wrong "stay compiled" decision is a silent
correctness failure (CLAUDE.md: guards are first-class). One enforcement
point, one loader, one guard implementation wins until there is a conformance
suite a JS loader must pass; recording that suite as the prerequisite.

**`node-ffi` / `koffi` over a plain C ABI cdylib.** No napi dependency, but
hand-rolled string marshalling and error signalling on both sides of the
boundary, a JS-side ffi dependency at runtime, and (for node-ffi) an
unmaintained loader. napi gives classes, string conversion, and structured
throws natively at zero JS-dependency cost; its price is a build-time Rust
dependency, paid once. Mirrors ADR-0024's ctypes rejection.

**Keep stdio; do not embed.** The 0.29 ms resident stdio runner already
amortizes compilation and stays the language-agnostic path. Rejected as the
end state for the same reason as ADR-0024: an in-process agent should not
shell out to itself; the remaining transport is the process boundary.

**WASI + `require('node:wasi')` to run the module directly.** A special case
of wasm-in-node with the same enforcement-point problem, plus Node's WASI is
experimental and the artifact ABI is not WASI-command-shaped.

## consequences

- A Node/TypeScript agent can hold a compiled `.cbin` resident and answer on
  the tier-1 fast path with a direct call. Per-call latency is measured by
  `bench.js`, per machine; the first measured run (numbers, provenance, and
  the fixture generator) is recorded in `evals/embedded-node/README.md`, well
  under the stdio floor. This ADR carries no number of its own.
- The three Rust gates (`cargo check|clippy|test -p auto-node`) pass without
  a Node toolchain (measured on rustc 1.96.1). Building the addon is
  operator-run: plain `cargo build -p auto-node --release --features node`
  produces a cdylib that `bench.js` loads directly via `process.dlopen` (a
  `.node` file is the same dylib renamed) — measured end-to-end on Windows
  (node v22.20.0): version, both error codes, the frozen refusal message,
  the abstention properties including `distance: null` for a wrong-shaped
  input. `@napi-rs/cli` adds renaming/`.d.ts`/cross-compilation but expects
  npm packaging this repo does not ship in v0.
- Twin alignment is a tested property, not a comment: `tests/twin_contract.rs`
  decodes envelopes emitted by the REAL `auto_runtime::Runner` over runnable
  in-memory artifacts (output, trip with/without distance, trap, bad input),
  and checks the load gate and the loader refuse a capability artifact
  alike — the differential-vs-reference norm applied to the binding.
- v0 embeds pure artifacts only; capability artifacts are refused loudly at
  load with the frozen ADR-0024 message.
- No npm package, no generated `.d.ts`, no prebuilds: the frozen surface is
  documented (lib.rs, README); packaging is recorded below.

## recorded follow-ups

- **`tools=` parity with auto-py (ADR-0027).** Embedded host tool callbacks
  mapping declared capabilities to JS functions, through the same `HostTools`
  loader rules (exactly-declared coverage). The v0 refusal names this.
- **wasm-in-node.** A pure-JS loader running the module in V8 — zero native
  install. Prerequisite: a loader/guard conformance suite so the second
  implementation cannot drift silently.
- **Async surface.** `AsyncTask`-based `answerAsync` for long-running
  artifacts, so the event loop is not blocked beyond microseconds.
- **npm packaging.** `package.json` + `@napi-rs/cli` builds, generated
  `.d.ts`, per-platform prebuilds, and the optional CI job below; publish
  alongside `.cbin`s and the auto-py wheels.
- **Shared embed-core crate.** Extract the twins' duplicated pure logic when
  a third embedding (or `tools=` parity here) lands.

## sources

- crates.io sparse index: napi 3.10.3, napi-derive 3.5.9, napi-build 2.3.2 —
  latest non-yanked stable; all `rust_version = 1.88`.
- napi-3.10.3 `src/error.rs`: `Error<S: AsRef<str> = Status>`;
  `impl_object_methods!` `into_value` passes `status.as_ref()` to
  `napi_create_error` (the JS `code`); `From<Unknown> for Error` +
  `ToNapiValue for Error` reuse the referenced JS object on throw.
  napi-3.10.3 `src/bindgen_runtime/js_values.rs`: `ToNapiValue for Option<T>`
  maps `None` to `null`.
- napi-derive-backend-5.1.1 `src/codegen/fn.rs`: constructor and sync-method
  `Err` arms both `JsError::from(err).throw_into(env)` (generic status);
  `Env`-typed args are injected, not read from JS.
- workspace toolchain: `rustc 1.96.1 (2026-06-26)`; node v22.20.0 on the
  build host.

## appendix: proposed optional CI job (not added by this change)

**LANDED (wave-13)**, together with the npm-packaging follow-up,
as the `embedded-node` job of `.github/workflows/embedded.yml` — a SEPARATE
workflow (ci.yml untouched), optional by convention: not a required check,
but the job itself is not `continue-on-error`. The packaging that shipped:
`crates/auto-node/package.json` (`private: true`, `napi.binaryName =
auto_node`, a `build` script driving `@napi-rs/cli@3` via npx — no
`npm install`, no lockfile), `.npmignore`, and a HAND-WRITTEN `index.d.ts`
(deliberately not the generated typedef: the generator cannot express the
thrown `code`s or the abstention properties, which are half the frozen
surface; the build script redirects the CLI's generated file to
`target/auto_node.generated.d.ts` so it never clobbers the checked-in one —
drift-checked against the generated output on the dev host, signatures
identical). The previously-unverified napi-cli route is now MEASURED
(Windows 11, node v22.20.0, @napi-rs/cli 3.7.2): `npm run
build` exits 0, emits `crates/auto-node/auto_node.node` (17,690,112 bytes),
and the full error-contract smoke passes through it. What shipped differs
from the sketch below (kept for history): ubuntu-only, node 22 (no OS/node
matrix — no prebuilds in v0, platform = build host); `checkout@v5` /
`setup-node@v5` per ci.yml conventions; flatc + rust-cache steps the sketch
omitted (auto-ir is in the dep tree, ADR-0001); the build goes through the
checked-in npm script rather than plain cargo; and the smoke `require()`s
`crates/auto-node/auto_node.node` directly — the CLI names the copy, so the
per-platform dlopen incantation below is no longer needed. Still open from
the follow-up: per-platform prebuilds and registry publish.

The original proposal (the Rust gates already run in the main matrix
without Node):

```yaml
  embedded-node:
    name: auto-node addon smoke (optional)
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, windows-latest, macos-latest]
        node-version: [20, 22]
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: ${{ matrix.node-version }}
      - uses: dtolnay/rust-toolchain@1.96.1
      - run: cargo build -p auto-node --release --features node
      # smoke: load the addon in-process and assert version() answers and a
      # missing artifact refuses with code AutoError. bench.js finds the raw
      # cdylib in target/release on its own.
      - run: >
          node -e "const p=require('path');const m={exports:{}};
          const c={win32:'auto_node.dll',darwin:'libauto_node.dylib',linux:'libauto_node.so'}[process.platform];
          process.dlopen(m,p.resolve('target/release',c));
          if(!m.exports.version())throw new Error('version() empty');
          try{new m.exports.Runner('does-not-exist.cbin');throw new Error('unreachable')}
          catch(e){if(e.code!=='AutoError')throw e}"
```

Kept optional because it is the only job needing a Node toolchain; the Rust
correctness gates do not.
