//! Recording proxy — record an agent WITHOUT changing its code.
//!
//! An OpenAI-compatible `/v1/chat/completions` endpoint that forwards each
//! request (with the caller's own Authorization header) to the real
//! upstream, relays the response verbatim, and ingests the exchange into a
//! trace store as a synthetic single-span `model_call` — with the reserved
//! `cost_usd_micros` / `tokens` attrs computed from the response usage and
//! the pinned price table (`auto_frontier::prices`; a model absent from the
//! table records tokens only and no cost — never a guessed price).
//!
//! The proxy holds NO key of its own and never originates paid calls: it
//! forwards the caller's credentials. Blocking `tiny_http` + `ureq`.
//!
//! This file holds the frozen seam (config + error types). The pure
//! request/response transformation and the socket loop are built in sibling
//! modules; transformation functions take parsed values so tests never open
//! a socket.

use std::path::PathBuf;

pub mod record;
pub mod server;

pub use record::{Exchange, exchange_to_trace};
pub use server::proxy;

/// Proxy configuration.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// upstream base, e.g. `https://api.openai.com`
    pub upstream: String,
    /// sqlite trace store to ingest recorded spans into
    pub store: PathBuf,
    /// bind address, e.g. `127.0.0.1:7434`
    pub addr: String,
    /// task name recorded on every ingested trace
    pub task: String,
}

/// Every honest way the proxy fails to start or serve.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("cannot open store at {store}: {detail}")]
    Store { store: String, detail: String },
    #[error("cannot bind {addr}: {detail}")]
    Bind { addr: String, detail: String },
    #[error("server loop failed: {detail}")]
    Loop { detail: String },
}
