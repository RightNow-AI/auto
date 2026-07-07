//! Tier-1 artifact server — the runtime as a long-running process.
//!
//! Serves artifacts out of a local registry over HTTP: every request is
//! guard-gated exactly like `auto run` (in-distribution answers tier-1; a
//! tripped guard ABSTAINS with an honest 409 — v0 has no in-server tier-0,
//! because deopt spend policy per request is an unresolved design, stated
//! not hidden). Blocking `tiny_http`, thread-per-request, no tokio.
//!
//! This file holds the frozen seam (config + error types). The pure request
//! handler and the socket loop are built in sibling modules; handlers are
//! pure functions over parsed requests so tests never open a socket.

pub mod api;
pub mod server;

pub use api::{ApiRequest, ApiResponse, Method, ServerState, handle};
pub use server::serve;

use std::path::PathBuf;

/// Server configuration — one registry, one bind address, and the operator's
/// server-wide tool table.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// registry root (`artifacts/<id>.cbin` layout, auto-registry)
    pub registry_root: PathBuf,
    /// bind address, e.g. `127.0.0.1:7433`
    pub addr: String,
    /// live tools as raw `name=command` flags (the `auto run --tool` grammar;
    /// ADR-0017 amendment, wave 7). Empty = a pure server (the pre-wave-7
    /// behavior). Parsed ONCE at startup into the single Live table every
    /// capability artifact loads through; the loader enforces per-artifact
    /// coverage. The operator chooses this table — requesters cannot.
    pub tools: Vec<String>,
    /// per-request tool-call budget (ADR-0028). `None` = today's behavior:
    /// unlimited tool execution per request. `Some(n)` wraps the operator's
    /// Live table in a counting host that audits every executed tool call and
    /// refuses the `n+1`-th call in a single request with an err envelope the
    /// artifact surfaces as a 500 — so a requester cannot drive unbounded
    /// side-effectful tool execution. A budget with no `--tool` table has
    /// nothing to count (vacuously satisfied).
    pub max_tool_calls_per_request: Option<u64>,
}

/// Every honest way the server fails to start or serve.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("cannot open registry at {root}: {detail}")]
    Registry { root: String, detail: String },
    #[error("cannot bind {addr}: {detail}")]
    Bind { addr: String, detail: String },
    #[error("server loop failed: {detail}")]
    Loop { detail: String },
    /// a malformed `--tool` flag at startup (the `name=command` grammar,
    /// ADR-0017); fail loud before binding rather than serve half-configured
    #[error("invalid --tool flag: {detail}")]
    Config { detail: String },
}
