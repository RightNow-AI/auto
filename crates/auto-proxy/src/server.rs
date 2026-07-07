//! The socket shell: bind, receive, forward, relay, record.
//!
//! Everything semantic is a pure function ([`preflight`] and
//! [`crate::record::exchange_to_trace`]); this module is the thin
//! `tiny_http` + `ureq` I/O layer that feeds them. No unit test here opens a
//! socket — the pure [`preflight`] is tested directly, and loopback proof
//! lives in the orchestrator's e2e scripts, not in `cargo test`.
//!
//! v0 shape (ADR-0012): single-threaded `recv` loop (one upstream call in
//! flight at a time — the store ingest is `&mut`); only the request body and
//! the caller's `Authorization` header are forwarded (other request headers
//! are dropped); streaming is refused. Recording is relay-first and
//! best-effort: an ingest failure is loud but never fails the agent's call.

use std::io::Cursor;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tiny_http::{Header, Request, Response, Server, StatusCode};

use auto_trace::Store;

use crate::record::{Exchange, exchange_to_trace};
use crate::{ProxyConfig, ProxyError};

/// The only route the proxy records.
const CHAT_PATH: &str = "/v1/chat/completions";
/// Upstream call ceiling (matches the frontier client, ADR-0010).
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(120);

// JSON bodies the proxy itself originates (never from the upstream).
const NOT_CHAT: &str = r#"{"error":"auto-proxy records POST /v1/chat/completions only"}"#;
const STREAM_REFUSED: &str = r#"{"error":"streaming is not recordable in v0"}"#;
const MISSING_AUTH: &str = r#"{"error":"missing Authorization header; auto-proxy forwards the caller's credentials and stores none"}"#;
const BODY_UNREADABLE: &str = r#"{"error":"auto-proxy could not read the request body"}"#;

/// Run the recording proxy until a fatal accept error. Opens the store first
/// (a bad store path fails before a socket is bound), then binds and serves.
pub fn proxy(config: ProxyConfig) -> Result<(), ProxyError> {
    let mut store = Store::open(&config.store).map_err(|e| ProxyError::Store {
        store: config.store.display().to_string(),
        detail: e.to_string(),
    })?;

    let server = Server::http(&config.addr).map_err(|e| ProxyError::Bind {
        addr: config.addr.clone(),
        detail: e.to_string(),
    })?;

    let agent = build_agent();

    eprintln!(
        "auto-proxy: recording POST {CHAT_PATH} on http://{} -> {} (store {}, task {:?})",
        config.addr,
        config.upstream,
        config.store.display(),
        config.task,
    );

    loop {
        match server.recv() {
            Ok(request) => {
                if let Err(e) = handle(&config, &agent, &mut store, request) {
                    // a socket write failure (the caller hung up) drops one
                    // exchange; it never ends the server
                    eprintln!("auto-proxy: dropped one exchange: {e}");
                }
            }
            Err(e) => {
                return Err(ProxyError::Loop {
                    detail: e.to_string(),
                });
            }
        }
    }
}

/// One shared blocking agent (rustls). `http_status_as_error(false)` so an
/// upstream 4xx/5xx body is read and relayed verbatim rather than collapsed
/// into a transport error (ADR-0010).
fn build_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(UPSTREAM_TIMEOUT))
        .build()
        .new_agent()
}

/// Handle one request: read it, preflight, and either reject or
/// forward-and-record. The only `Err` returned is a socket write failure.
fn handle(
    config: &ProxyConfig,
    agent: &ureq::Agent,
    store: &mut Store,
    mut request: Request,
) -> std::io::Result<()> {
    let method = request.method().to_string();
    let path = request.url().split('?').next().unwrap_or("").to_owned();
    let authorization = find_header(&request, "Authorization");

    let mut body = Vec::new();
    if let Err(e) = request.as_reader().read_to_end(&mut body) {
        eprintln!("auto-proxy: {method} {path} -> 400 (unreadable request body: {e})");
        return request.respond(json_response(400, BODY_UNREADABLE));
    }

    match preflight(&method, &path, authorization.is_some(), &body) {
        Preflight::Reject { status, body } => {
            eprintln!("auto-proxy: {method} {path} -> {status} (not forwarded)");
            request.respond(json_response(status, &body))
        }
        Preflight::Forward => match authorization {
            Some(auth) => forward_and_record(config, agent, store, body, &auth, request),
            // preflight only forwards when auth is present; degrade, never panic
            None => request.respond(json_response(401, MISSING_AUTH)),
        },
    }
}

/// The routing decision, computed purely from the parsed request — no socket,
/// no upstream. Order: route (404) → streaming refusal (400) → auth (401) →
/// forward. Unit-tested directly.
#[derive(Debug, PartialEq, Eq)]
pub enum Preflight {
    /// refuse before forwarding: HTTP status + a JSON body to relay back
    Reject { status: u16, body: String },
    /// forward the body upstream unchanged
    Forward,
}

/// Decide what to do with an incoming request. Pure over the parsed inputs.
pub fn preflight(method: &str, path: &str, has_auth: bool, body: &[u8]) -> Preflight {
    if !(method.eq_ignore_ascii_case("POST") && path == CHAT_PATH) {
        return Preflight::Reject {
            status: 404,
            body: NOT_CHAT.to_owned(),
        };
    }
    if body_requests_stream(body) {
        return Preflight::Reject {
            status: 400,
            body: STREAM_REFUSED.to_owned(),
        };
    }
    if !has_auth {
        return Preflight::Reject {
            status: 401,
            body: MISSING_AUTH.to_owned(),
        };
    }
    Preflight::Forward
}

/// True iff the body is a JSON object with `"stream": true`. A body that is
/// not JSON is left for the upstream to reject (relay-first — the proxy does
/// not second-guess request validity beyond what it must to record).
fn body_requests_stream(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// Forward the body upstream with the caller's `Authorization`, relay the
/// response verbatim, then (2xx only) record the exchange. Relay-first: the
/// caller's response is sent before the store is touched, and a recording
/// failure is logged but never propagated to the caller.
fn forward_and_record(
    config: &ProxyConfig,
    agent: &ureq::Agent,
    store: &mut Store,
    body: Vec<u8>,
    authorization: &str,
    request: Request,
) -> std::io::Result<()> {
    let url = format!(
        "{}/v1/chat/completions",
        config.upstream.trim_end_matches('/')
    );
    let model = model_of(&body);
    let started_at_ms = unix_now_ms();
    let started = Instant::now();

    // forward the caller's credentials verbatim; the proxy holds no key
    let outcome = agent
        .post(&url)
        .header("authorization", authorization)
        .header("content-type", "application/json")
        .send(body.as_slice());
    let duration_ms = elapsed_ms(started);

    let (status, resp_bytes) = match outcome {
        Ok(mut response) => {
            let status = response.status().as_u16();
            match response.body_mut().read_to_vec() {
                Ok(bytes) => (status, bytes),
                Err(e) => {
                    eprintln!(
                        "auto-proxy: model={model} status={status} {duration_ms}ms upstream body unreadable: {e}"
                    );
                    let msg = json_error(&format!("auto-proxy: upstream body unreadable: {e}"));
                    return request.respond(json_response(502, &msg));
                }
            }
        }
        Err(e) => {
            eprintln!("auto-proxy: model={model} upstream call failed: {e}");
            return request.respond(json_response(
                502,
                &json_error("auto-proxy: upstream request failed"),
            ));
        }
    };

    // relay-first: return the upstream bytes to the caller before the store is
    // touched, so recording never adds latency to — or fails — the agent's call
    let relay = Response::from_data(resp_bytes.clone())
        .with_status_code(StatusCode(status))
        .with_header(json_content_type());
    let relayed = request.respond(relay);

    // ingest-second: 2xx only; every failure is loud but swallowed
    let ingested = if (200..300).contains(&status) {
        match record_exchange(
            store,
            &config.task,
            &body,
            &resp_bytes,
            started_at_ms,
            duration_ms,
        ) {
            Ok(trace_id) => format!("ingested {trace_id}"),
            Err(e) => format!("NOT ingested: {e}"),
        }
    } else {
        "not recorded (non-2xx)".to_owned()
    };
    eprintln!("auto-proxy: model={model} status={status} {duration_ms}ms {ingested}");

    relayed
}

/// Parse both bodies, build the [`Exchange`], and ingest it. Returns the trace
/// id on success. Every failure is a `String` for the caller to log — never
/// propagated to the relay path.
fn record_exchange(
    store: &mut Store,
    task: &str,
    request_body: &[u8],
    response_body: &[u8],
    started_at_ms: u64,
    duration_ms: u64,
) -> Result<String, String> {
    let request_body: Value = serde_json::from_slice(request_body)
        .map_err(|e| format!("request body is not JSON: {e}"))?;
    let response_body: Value = serde_json::from_slice(response_body)
        .map_err(|e| format!("response body is not JSON: {e}"))?;
    let exchange = Exchange {
        request_body,
        response_body,
        duration_ms,
        started_at_ms,
    };
    let trace = exchange_to_trace(task, &exchange, std::process::id())?;
    let trace_id = trace.header.trace_id.to_string();
    store.ingest(&trace).map_err(|e| e.to_string())?;
    Ok(trace_id)
}

/// The request's declared model, for the per-exchange log line; "?" when the
/// body is absent, unparseable, or has no string `model`.
fn model_of(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "?".to_owned())
}

/// The value of the first header whose field equals `name` (ASCII
/// case-insensitive), owned.
fn find_header(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_owned())
}

/// `Content-Type: application/json` — a static, always-valid header.
fn json_content_type() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("a static, valid header never fails to parse")
}

/// A proxy-originated JSON response with a status code.
fn json_response(status: u16, body: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_string(body.to_owned())
        .with_status_code(StatusCode(status))
        .with_header(json_content_type())
}

/// Build an `{"error": message}` body with the message JSON-escaped, so a
/// stray quote in an upstream/transport error string can never break the body.
fn json_error(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_chat_with_auth_and_no_stream_forwards() {
        let body = br#"{"model":"gpt-5.4-mini","messages":[]}"#;
        assert_eq!(preflight("POST", CHAT_PATH, true, body), Preflight::Forward);
    }

    #[test]
    fn wrong_method_or_path_is_404() {
        let body = b"{}";
        assert!(matches!(
            preflight("GET", CHAT_PATH, true, body),
            Preflight::Reject { status: 404, .. }
        ));
        assert!(matches!(
            preflight("POST", "/v1/embeddings", true, body),
            Preflight::Reject { status: 404, .. }
        ));
    }

    #[test]
    fn method_check_is_case_insensitive() {
        let body = br#"{"model":"m"}"#;
        assert_eq!(preflight("post", CHAT_PATH, true, body), Preflight::Forward);
    }

    #[test]
    fn query_string_does_not_defeat_the_path_match() {
        // the socket loop strips the query before calling preflight
        let body = br#"{"model":"m"}"#;
        assert_eq!(preflight("POST", CHAT_PATH, true, body), Preflight::Forward);
        assert!(matches!(
            preflight("POST", "/v1/chat/completions/extra", true, body),
            Preflight::Reject { status: 404, .. }
        ));
    }

    #[test]
    fn streaming_is_refused_before_forwarding() {
        let body = br#"{"model":"m","stream":true}"#;
        assert!(matches!(
            preflight("POST", CHAT_PATH, true, body),
            Preflight::Reject { status: 400, .. }
        ));
    }

    #[test]
    fn stream_false_forwards() {
        let body = br#"{"model":"m","stream":false}"#;
        assert_eq!(preflight("POST", CHAT_PATH, true, body), Preflight::Forward);
    }

    #[test]
    fn missing_auth_is_401() {
        let body = br#"{"model":"m"}"#;
        assert!(matches!(
            preflight("POST", CHAT_PATH, false, body),
            Preflight::Reject { status: 401, .. }
        ));
    }

    #[test]
    fn non_json_body_still_forwards_and_the_upstream_rejects_it() {
        // relay-first: the proxy does not pre-validate JSON
        assert_eq!(
            preflight("POST", CHAT_PATH, true, b"not json at all"),
            Preflight::Forward
        );
    }

    #[test]
    fn streaming_refusal_precedes_the_auth_check() {
        // documents the order: a streaming request with no auth gets 400, not 401
        let body = br#"{"stream":true}"#;
        assert!(matches!(
            preflight("POST", CHAT_PATH, false, body),
            Preflight::Reject { status: 400, .. }
        ));
    }
}
