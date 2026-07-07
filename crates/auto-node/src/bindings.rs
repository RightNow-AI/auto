//! The napi surface: the `auto_node` addon, its `Runner` class, the two error
//! codes, and `version()`. All N-API glue lives here so the rest of the crate
//! stays napi-free. Compiled only with the `node` feature and never into the
//! unit-test binary (see lib.rs).
//!
//! Error contract (ADR-0026, frozen): every failure is a thrown JS `Error`
//! whose `code` property distinguishes the two outcomes — `"AutoError"` for
//! load/parse/execution failures, `"AutoAbstained"` for a guard trip, the
//! latter carrying `reason` (string | null), `distance` (number | null), and
//! `threshold` (number | null) as own properties. Mechanics, verified against
//! the pinned napi 3.10.3 source:
//!
//! - a custom status type ([`ErrorCode`], any `AsRef<str>`) becomes the thrown
//!   error's `code`: the derive's `Err` arm does `JsError::from(err)
//!   .throw_into(env)`, and `JsError::into_value` passes the status string to
//!   `napi_create_error` as the code (napi-3.10.3 `src/error.rs`,
//!   `impl_object_methods!`). The constructor uses this path directly.
//! - structured properties need a real JS object, so [`abstained_error`]
//!   builds the error object first, sets the three properties, and wraps it in
//!   `napi::Error::from(Unknown)` — which holds a `napi_ref` to that exact
//!   object, and the throw path REUSES the referenced object ("keeps its
//!   subclass, stack, and own properties", napi-3.10.3 `src/error.rs`
//!   `ToNapiValue for Error`) rather than rebuilding from status/reason.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::logic::{Abstention, Decoded, capability_refusal_message, decode_answer};

/// Custom napi error status: the string becomes the thrown JS error's `code`.
/// `pub` only because the pub constructor's `Result` names it — the `bindings`
/// module itself is private, so nothing leaks past the crate.
#[derive(Debug, Clone, Copy)]
pub enum ErrorCode {
    /// artifact load or tier-1 parse/execution failure
    AutoError,
    /// the runtime guard tripped; tier-1 abstained rather than answer out of
    /// distribution (there is no in-process tier-0, spec/runtime.md §9)
    AutoAbstained,
}

impl AsRef<str> for ErrorCode {
    fn as_ref(&self) -> &str {
        match self {
            ErrorCode::AutoError => "AutoError",
            ErrorCode::AutoAbstained => "AutoAbstained",
        }
    }
}

/// An `Error` that throws as `code === "AutoError"` with this detail.
fn auto_error<D: ToString>(detail: D) -> Error<ErrorCode> {
    Error::new(ErrorCode::AutoError, detail.to_string())
}

/// Build the thrown-object form of a coded error: a real JS `Error` instance
/// with `code` and `message` set, wrapped in a `napi::Error` that re-throws
/// that exact object (it holds a `napi_ref`; the throw path reuses the
/// referenced object, so properties set on it survive). Needed where the
/// method's `Result` error type is the plain `napi::Error` but the thrown
/// object must still carry our `code` — and, for abstentions, the structured
/// guard fields.
fn coded_error_object(env: Env, code: ErrorCode, message: &str) -> Result<Object<'_>> {
    let unknown = JsError::from(Error::new(code, message)).into_unknown(env);
    unknown.coerce_to_object()
}

/// The `AutoAbstained` error: message = the composed guard detail; `reason` /
/// `distance` / `threshold` ride as own properties (`Option::None` converts
/// to JS `null` — napi-3.10.3 `ToNapiValue for Option<T>`).
fn abstained_error(env: Env, abstention: &Abstention) -> Result<Error> {
    let mut object = coded_error_object(env, ErrorCode::AutoAbstained, &abstention.message)?;
    object.set_named_property("reason", abstention.reason.clone())?;
    object.set_named_property("distance", abstention.distance)?;
    object.set_named_property("threshold", abstention.threshold)?;
    Ok(Error::from(object.to_unknown()))
}

/// The `AutoError` error in thrown-object form, for `answer`'s failure arm.
fn execution_error(env: Env, detail: &str) -> Result<Error> {
    let object = coded_error_object(env, ErrorCode::AutoError, detail)?;
    Ok(Error::from(object.to_unknown()))
}

/// One compiled artifact held resident in the host Node process. The wasm
/// module is compiled once here; `.answer` runs a fresh instance per call
/// (the frozen one-`run`-per-instance ABI — no cross-call state leaks).
#[napi]
pub struct Runner {
    inner: auto_runtime::Runner,
}

#[napi]
impl Runner {
    /// Load `artifactPath` and compile the module once. Every failure throws
    /// with `code === "AutoError"`.
    ///
    /// Pure-only v0 (mirroring ADR-0024 decision 4): a capability-bearing
    /// artifact refuses at LOAD with the frozen ADR-0024 message — not a
    /// surprise at call time. The auto-py twin's `tools=` host (ADR-0027) is
    /// a recorded follow-up for this twin (ADR-0026).
    #[napi(constructor)]
    pub fn new(artifact_path: String) -> Result<Runner, ErrorCode> {
        let bytes = std::fs::read(&artifact_path)
            .map_err(|e| auto_error(format!("cannot read artifact {artifact_path:?}: {e}")))?;
        // Parse the manifest HERE — before handing bytes to the runner — so
        // the refusal carries the honest embedded-host message rather than
        // the loader's generic missing-tools text; the loader still re-runs
        // every cross-check.
        let artifact = auto_backend::Artifact::from_bytes(&bytes)
            .map_err(|e| auto_error(format!("invalid artifact: {e}")))?;
        let manifest = artifact
            .manifest()
            .map_err(|e| auto_error(format!("invalid manifest: {e}")))?;
        if let Some(message) = capability_refusal_message(&manifest.capabilities) {
            return Err(auto_error(message));
        }
        let inner = auto_runtime::Runner::new(&bytes).map_err(auto_error)?;
        Ok(Runner { inner })
    }

    /// Answer one input. Returns the tier-1 output value as canonical JSON
    /// text; throws `code === "AutoAbstained"` on a guard trip (message +
    /// `reason` / `distance` / `threshold` properties) and
    /// `code === "AutoError"` on any parse/execution failure.
    ///
    /// Synchronous ON the JS thread — the event loop blocks for the duration
    /// of the wasm call (microseconds for compiled artifacts; that is the
    /// point). An async surface is a recorded follow-up (ADR-0026).
    #[napi]
    pub fn answer(&self, env: Env, input_json: String) -> Result<String> {
        match decode_answer(&self.inner.answer(&input_json)) {
            Decoded::Output(output) => Ok(output),
            // the inner `?` surfaces a (never-expected) failure to BUILD the
            // coded error object as its own napi error rather than a panic
            Decoded::Abstained(abstention) => Err(abstained_error(env, &abstention)?),
            Decoded::Error(detail) => Err(execution_error(env, &detail)?),
        }
    }
}

/// The crate version, exposed to JS as `version()`.
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}
