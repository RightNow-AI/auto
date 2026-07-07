//! The socket shell around the pure [`crate::api`] core.
//!
//! [`serve`] opens the registry, binds `tiny_http`, and runs a **blocking,
//! sequential** accept loop: each request is parsed into an
//! [`ApiRequest`](crate::api::ApiRequest) (body read fully into memory),
//! answered by [`handle`], and written back as `application/json`, with one
//! log line per request. Sequential is the deliberate v0 choice —
//! correctness first; thread-per-request is a recorded upgrade (ADR-0011).
//! No sockets are opened in this crate's tests; loopback integration is the
//! orchestrator's e2e job.

use auto_registry::Registry;
use serde_json::json;
use tiny_http::{Header, Method as HttpMethod, Request, Response, Server};

use crate::api::{ApiRequest, ApiResponse, Method, ServerState, handle, parse_tool_table};
use crate::{ServeConfig, ServeError};

/// Open the registry, bind the address, and serve until the process is
/// stopped. Returns only on a socket-level accept failure
/// ([`ServeError::Loop`]) — the normal lifetime of this call is "until
/// killed"; a clean shutdown path is future work.
pub fn serve(config: ServeConfig) -> Result<(), ServeError> {
    // parse the operator's tool table FIRST: a malformed --tool flag fails
    // loud before we touch the registry or bind a socket (ADR-0017).
    let tools = parse_tool_table(&config.tools)?;

    let registry = Registry::open(&config.registry_root).map_err(|e| ServeError::Registry {
        root: config.registry_root.display().to_string(),
        detail: e.to_string(),
    })?;
    let mut state =
        ServerState::with_tool_policy(registry, tools, config.max_tool_calls_per_request);

    let server = Server::http(config.addr.as_str()).map_err(|e| ServeError::Bind {
        addr: config.addr.clone(),
        detail: e.to_string(),
    })?;
    eprintln!(
        "auto serve: listening on {} (registry {}); tier-1 only, abstain-not-deopt; {}; {}",
        config.addr,
        config.registry_root.display(),
        if config.tools.is_empty() {
            "no tools (pure artifacts only)".to_owned()
        } else {
            format!(
                "{} tool flag(s) (capability artifacts served through the table)",
                config.tools.len()
            )
        },
        match config.max_tool_calls_per_request {
            Some(n) => format!("tool budget {n}/request (ADR-0028)"),
            None => "no per-request tool budget (unlimited)".to_owned(),
        },
    );

    loop {
        let mut request = match server.recv() {
            Ok(request) => request,
            Err(e) => {
                return Err(ServeError::Loop {
                    detail: e.to_string(),
                });
            }
        };

        // capture the log label and path before the body borrow
        let label = method_label(request.method());
        let path = request.url().to_owned();

        let Some(method) = map_method(request.method()) else {
            let response = ApiResponse {
                status: 405,
                body: json!({ "error": format!("method {label} not supported") }),
            };
            eprintln!("{label} {path} -> {}", response.status);
            respond(request, response);
            continue;
        };

        let mut body = Vec::new();
        if let Err(e) = request.as_reader().read_to_end(&mut body) {
            let response = ApiResponse {
                status: 400,
                body: json!({ "error": format!("could not read request body: {e}") }),
            };
            eprintln!("{label} {path} -> {}", response.status);
            respond(request, response);
            continue;
        }

        let api_request = ApiRequest { method, path, body };
        let response = handle(&mut state, &api_request);
        eprintln!("{label} {} -> {}", api_request.path, response.status);
        respond(request, response);
    }
}

/// Map `tiny_http`'s method onto the two the core routes; anything else is
/// refused (405) by the caller.
fn map_method(method: &HttpMethod) -> Option<Method> {
    match method {
        HttpMethod::Get => Some(Method::Get),
        HttpMethod::Post => Some(Method::Post),
        _ => None,
    }
}

/// A log/refusal label for any method — `GET`/`POST` cleanly, others via
/// `Debug` (enough for a log line and an error string).
fn method_label(method: &HttpMethod) -> String {
    match method {
        HttpMethod::Get => "GET".to_owned(),
        HttpMethod::Post => "POST".to_owned(),
        other => format!("{other:?}"),
    }
}

/// Serialize the response body and write it with its status code and
/// `application/json`. A send failure (client hung up) is logged, not fatal.
fn respond(request: Request, response: ApiResponse) {
    let body = serde_json::to_vec(&response.body).expect("a serde_json::Value always serializes");
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("a static, valid Content-Type header");
    let http = Response::from_data(body)
        .with_status_code(response.status)
        .with_header(header);
    if let Err(e) = request.respond(http) {
        eprintln!("auto serve: could not send response: {e}");
    }
}
