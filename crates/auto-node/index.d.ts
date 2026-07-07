// auto-node — hand-written type definitions for the frozen ADR-0026 surface.
//
// HAND-WRITTEN, not @napi-rs/cli output: the generated typedef cannot carry
// the error contract (thrown `code`s and the abstention properties), which is
// half the API. The npm `build` script redirects the CLI's generated file into
// the gitignored target/ dir so it never clobbers this one; if the frozen
// surface in crates/auto-node/src/bindings.rs ever changes, this file changes
// in the same commit or the twin-contract review catches the drift.
//
// Error contract (frozen): every failure is a thrown JS `Error` distinguished
// by its `code` property — NOT by `instanceof` (a Node addon does not export
// subclass identity across contexts; `code` is the Node convention and
// survives serialization). See `AutoError` / `AutoAbstained` below.

/**
 * One compiled `.cbin` artifact held resident in the host Node.js process.
 *
 * The wasm module is compiled once, at construction; each `.answer` call runs
 * a fresh wasm instance (the frozen one-`run`-per-instance ABI — no
 * cross-call state leaks) reached by a direct function call: no subprocess,
 * no HTTP, no stdio.
 */
export declare class Runner {
  /**
   * Read `artifactPath`, parse + cross-check the container and manifest, and
   * compile the wasm module once.
   *
   * Throws `code === "AutoError"` on every load failure: unreadable path,
   * invalid container or manifest, a module that will not compile — and, in
   * v0, any capability-bearing artifact (nonempty manifest `capabilities`),
   * refused at LOAD with the frozen message "capability artifacts are not
   * supported embedded in v0 (recorded: per-request tool policy + host
   * callbacks)". The auto-py twin's `tools=` host callbacks (ADR-0027) are a
   * recorded follow-up for this twin.
   */
  constructor(artifactPath: string)

  /**
   * Answer one input: a single JSON value in (as text), the tier-1 OUTPUT
   * value out as canonical JSON text.
   *
   * The runner's other two outcomes (spec/runtime.md §9) are thrown:
   * - guard trip → `code === "AutoAbstained"` (see {@link AutoAbstained});
   *   there is no in-process tier-0 — an abstention never deopts.
   * - parse/execution failure → `code === "AutoError"`.
   *
   * Synchronous ON the JS thread: the event loop blocks for the duration of
   * the wasm call (microseconds for compiled artifacts — that is the point).
   * An async surface for long-running artifacts is a recorded follow-up
   * (ADR-0026).
   */
  answer(inputJson: string): string
}

/** This addon's crate version (`CARGO_PKG_VERSION`). */
export declare function version(): string

/**
 * The thrown shape of an artifact load failure or a tier-1 parse/execution
 * failure. Type-only: the addon exports no error class — discriminate with
 * `err.code === "AutoError"`, never `instanceof`.
 */
export interface AutoError extends Error {
  code: 'AutoError'
}

/**
 * The thrown shape of a guard trip: tier-1 abstained rather than answer out
 * of distribution. `message` carries the composed guard detail; the raw
 * fields ride as own properties. Type-only: discriminate with
 * `err.code === "AutoAbstained"`, never `instanceof`.
 */
export interface AutoAbstained extends Error {
  code: 'AutoAbstained'
  /** the guard's stated reason; `null` only for a malformed envelope */
  reason: string | null
  /**
   * measured embedding distance; `null` for a wrong-shaped input with no
   * text to measure
   */
  distance: number | null
  /** the calibrated abstention threshold; `null` only for a malformed envelope */
  threshold: number | null
}
