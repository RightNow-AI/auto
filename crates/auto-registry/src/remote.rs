//! Remote transport for the content-addressed registry — ADR-0022, v0.
//!
//! Serves a local registry root over loopback HTTP and pushes/pulls artifacts
//! plus their detached ed25519 signatures across the wire. The registry stays
//! content-addressed on both ends: an id is the sha-256 of the canonical
//! container bytes, so tamper evidence is *structural* — it survives the wire
//! because either end recomputes the id from the bytes it holds and refuses a
//! mismatch. Signatures ride the same transport; sigstore keyless signing is
//! the recorded production target this is the transport for.
//!
//! Shape mirrors `auto-serve`: [`RegistryHost::handle`] is a pure request core
//! (method, path, body) -> ([`Reply`]) with **no socket**, so every route is
//! unit-tested without binding a port; [`serve`] is the thin `tiny_http` shell
//! around it. The client half ([`push`] / [`pull`]) is blocking `ureq`, the
//! same pins the frontier client and the recording proxy use.
//!
//! Not here (loud, honest bounds — ADR-0022): **no auth, no TLS**. This is a
//! development transport for a loopback / trusted-LAN registry. Authentication,
//! transport encryption, and sigstore keyless signing are recorded targets, not
//! shipped. The wire protocol is frozen in `spec/registry.md`.
//!
//! `remote` is part of `auto-registry`, not a client of it: it reuses the
//! crate's own layout constants and id/hex/signature helpers so the wire and
//! the on-disk store agree byte-for-byte on what an id is, how a signature is
//! encoded, and where files live.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use auto_backend::Artifact;
use ed25519_dalek::{PUBLIC_KEY_LENGTH, Signature, VerifyingKey};

use crate::{
    ARTIFACT_EXT, ARTIFACTS_DIR, KEYS_DIR, PUB_FILE, Registry, RegistryError, hex_decode,
    hex_encode, is_content_id, write_via_tmp,
};

/// The API version prefix every route carries.
const API: &str = "/v0";
const TEXT: &str = "text/plain; charset=utf-8";
const OCTET: &str = "application/octet-stream";
/// Client-side upstream ceiling (loopback; a stuck peer must not hang a CLI).
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// wire types
// ---------------------------------------------------------------------------

/// The HTTP verbs the transport routes on. The socket shell maps
/// `tiny_http::Method` into this; everything that is not GET or PUT becomes
/// [`Verb::Other`] and lands on a 404 (the protocol lists exact combinations —
/// anything else is not found).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Get,
    Put,
    Other,
}

/// A pure response: an HTTP status, a content type, and the body bytes. The
/// shell writes these back verbatim. Errors are `text/plain`; payloads are
/// `application/octet-stream`; the listing is `text/plain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Reply {
    fn octet(status: u16, body: Vec<u8>) -> Reply {
        Reply {
            status,
            content_type: OCTET,
            body,
        }
    }

    fn text(status: u16, msg: impl Into<String>) -> Reply {
        Reply {
            status,
            content_type: TEXT,
            body: msg.into().into_bytes(),
        }
    }
}

/// Every honest way the client half or the socket loop fails.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A local registry operation failed (open / add / get) — carries the
    /// underlying store error, including its tamper-evidence variants.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// Transport-level failure: connection refused, timeout, unreadable body.
    #[error("transport error: {detail}")]
    Http { detail: String },
    /// The server answered with an unexpected HTTP status; `detail` carries the
    /// server's own message body.
    #[error("server returned HTTP {code}: {detail}")]
    Status { code: u16, detail: String },
    /// The remote had no such artifact (404).
    #[error("artifact `{id}` not found on the remote")]
    NotFound { id: String },
    /// Pulled bytes do not hash to the id that was requested — tamper evidence
    /// caught at the receiving end. Nothing is written.
    #[error(
        "content digest mismatch (tamper evidence): requested `{expected}`, bytes hash to `{actual}`; nothing written"
    )]
    DigestMismatch { expected: String, actual: String },
    /// A pulled signature did not verify against the remote's verifying key.
    /// Nothing is written.
    #[error("signature for `{id}` did not verify (tamper evidence): {detail}; nothing written")]
    BadSignature { id: String, detail: String },
    /// The local root already trusts a different verifying key than the remote;
    /// pull refuses to overwrite a local trust root.
    #[error("verifying-key conflict: {detail}")]
    KeyConflict { detail: String },
    /// A malformed argument (non-64-hex id, un-decodable key/signature bytes).
    #[error("invalid: {detail}")]
    Invalid { detail: String },
    /// The `tiny_http` server could not bind its address.
    #[error("cannot bind {addr}: {detail}")]
    Bind { addr: String, detail: String },
    /// The accept loop failed at the socket level.
    #[error("server loop failed: {detail}")]
    Loop { detail: String },
}

// ---------------------------------------------------------------------------
// server side: the pure handler + the socket shell
// ---------------------------------------------------------------------------

/// A registry exposed over HTTP: the opened [`Registry`] (verified reads and
/// writes) plus its root path (raw reads of the detached `.sig` files and the
/// `keys/auto.pub` the store has no in-memory getter for — the on-disk layout
/// is the registry's documented contract).
pub struct RegistryHost {
    root: PathBuf,
    registry: Registry,
}

impl RegistryHost {
    /// Open the registry at `root` (creating `artifacts/` and `keys/` if
    /// needed, exactly like [`Registry::open`]).
    pub fn open(root: &Path) -> Result<RegistryHost, RegistryError> {
        Ok(RegistryHost {
            registry: Registry::open(root)?,
            root: root.to_path_buf(),
        })
    }

    /// Route and answer one request. Total and socket-free: every method/path
    /// lands on exactly one [`Reply`]. Reads re-verify before serving; writes
    /// re-derive the content id from the bytes and refuse a mismatch.
    pub fn handle(&self, method: Verb, raw_path: &str, body: &[u8]) -> Reply {
        // a query string is not part of the route
        let path = raw_path.split('?').next().unwrap_or("");

        if path == format!("{API}/key") {
            return match method {
                Verb::Get => self.get_key(),
                _ => not_found(),
            };
        }
        if path == format!("{API}/artifacts") {
            return match method {
                Verb::Get => self.list(),
                _ => not_found(),
            };
        }
        if let Some(rest) = path.strip_prefix(&format!("{API}/artifacts/")) {
            let (id, want_sig) = match rest.strip_suffix("/signature") {
                Some(id) => (id, true),
                None => (rest, false),
            };
            // malformed ids never name a stored artifact and must not traverse
            // paths — a hard 400 before any filesystem touch
            if !is_content_id(id) {
                return Reply::text(400, format!("malformed id `{id}` (want 64 lowercase hex)"));
            }
            return match (method, want_sig) {
                (Verb::Get, false) => self.get_artifact(id),
                (Verb::Get, true) => self.get_signature(id),
                (Verb::Put, false) => self.put_artifact(id, body),
                (Verb::Put, true) => self.put_signature(id, body),
                _ => not_found(),
            };
        }
        not_found()
    }

    /// `GET /v0/artifacts` — one 64-hex id per line, sorted, each terminated by
    /// a newline. Only well-formed content ids are listed; a stray non-id file
    /// is not advertised (a client GET on it would 500 anyway). A registry that
    /// cannot be listed (e.g. a corrupt public key) is a 500, never a blank OK.
    fn list(&self) -> Reply {
        match self.registry.list() {
            Ok(entries) => {
                let mut body = String::new();
                for entry in entries.iter().filter(|e| is_content_id(&e.id)) {
                    body.push_str(&entry.id);
                    body.push('\n');
                }
                Reply::text(200, body)
            }
            Err(e) => Reply::text(500, format!("registry list failed: {e}")),
        }
    }

    /// `GET /v0/artifacts/<id>` — verified artifact bytes. [`Registry::get`]
    /// re-parses, recomputes the content id, and verifies any signature before
    /// a single byte is served, so a corrupt or tampered entry is a 500 with
    /// the error, never silent bytes.
    fn get_artifact(&self, id: &str) -> Reply {
        let tmp = temp_path("get");
        let reply = match self.registry.get(id, &tmp) {
            Ok(_verified) => match std::fs::read(&tmp) {
                Ok(bytes) => Reply::octet(200, bytes),
                Err(e) => Reply::text(500, format!("read verified bytes: {e}")),
            },
            Err(RegistryError::NotFound { .. }) => Reply::text(404, format!("no artifact `{id}`")),
            Err(e) => Reply::text(
                500,
                format!("registry refused to serve `{id}` (corrupt entry, not served): {e}"),
            ),
        };
        let _ = std::fs::remove_file(&tmp);
        reply
    }

    /// `GET /v0/artifacts/<id>/signature` — the detached signature bytes as
    /// stored (hex), or 404 when the artifact is unsigned. The bytes are served
    /// raw; the puller verifies them against `GET /v0/key`. (Serving is safe:
    /// `GET /v0/artifacts/<id>` already refuses an artifact whose stored
    /// signature does not verify, and a pull fetches the artifact first.)
    fn get_signature(&self, id: &str) -> Reply {
        match std::fs::read(sig_file(&self.root, id)) {
            Ok(bytes) => Reply::octet(200, bytes),
            Err(e) if is_not_found(&e) => Reply::text(404, format!("no signature for `{id}`")),
            Err(e) => Reply::text(500, format!("read signature: {e}")),
        }
    }

    /// `GET /v0/key` — the registry's verifying key, in the same hex encoding
    /// [`Registry`] stores in `keys/auto.pub`. 404 when the registry has no key.
    fn get_key(&self) -> Reply {
        match std::fs::read(pub_file(&self.root)) {
            Ok(bytes) => Reply::octet(200, bytes),
            Err(e) if is_not_found(&e) => Reply::text(404, "this registry has no verifying key"),
            Err(e) => Reply::text(500, format!("read key: {e}")),
        }
    }

    /// `PUT /v0/artifacts/<id>` — store an uploaded artifact by content id.
    ///
    /// The server recomputes the content id from the body (via the strict
    /// container parse, which equals sha-256 of the body for every artifact the
    /// store accepts) and refuses (400) if it differs from the path id, writing
    /// nothing. Content-addressed: identical bytes already present are a 200
    /// idempotent no-op; a first store is 201; an id already present with
    /// *different* bytes is 409 (tamper evidence on the server store).
    fn put_artifact(&self, id: &str, body: &[u8]) -> Reply {
        let artifact = match Artifact::from_bytes(body) {
            Ok(artifact) => artifact,
            Err(e) => return Reply::text(400, format!("not a valid artifact: {e}")),
        };
        let actual = artifact.id();
        if actual != id {
            return Reply::text(
                400,
                format!(
                    "digest mismatch: body content id `{actual}` != path `{id}`; nothing written"
                ),
            );
        }
        let existed = artifact_file(&self.root, id).exists();
        let tmp = temp_path("put");
        if let Err(e) = std::fs::write(&tmp, body) {
            return Reply::text(500, format!("stage upload: {e}"));
        }
        let reply = match self.registry.add(&tmp, false) {
            Ok(_outcome) => {
                if existed {
                    Reply::text(200, format!("{id} present (idempotent)"))
                } else {
                    Reply::text(201, format!("{id} stored"))
                }
            }
            Err(RegistryError::IdMismatch { .. }) => Reply::text(
                409,
                format!("`{id}` already present with different bytes (tamper evidence); refused"),
            ),
            Err(RegistryError::InvalidArtifact(detail)) => {
                Reply::text(400, format!("not a valid artifact: {detail}"))
            }
            Err(e) => Reply::text(500, format!("registry add failed: {e}")),
        };
        let _ = std::fs::remove_file(&tmp);
        reply
    }

    /// `PUT /v0/artifacts/<id>/signature` — accept a detached signature only if
    /// the artifact exists AND the signature verifies against the registry's
    /// own verifying key over the stored artifact bytes; otherwise 400, storing
    /// nothing. The stored signature is re-encoded canonically (hex + newline),
    /// so a later `GET /v0/artifacts/<id>` verifies it exactly as the local
    /// store would.
    fn put_signature(&self, id: &str, body: &[u8]) -> Reply {
        let artifact_bytes = match std::fs::read(artifact_file(&self.root, id)) {
            Ok(bytes) => bytes,
            Err(e) if is_not_found(&e) => {
                return Reply::text(
                    400,
                    format!("no artifact `{id}`; PUT the artifact before its signature"),
                );
            }
            Err(e) => return Reply::text(500, format!("read artifact: {e}")),
        };
        let pub_hex = match std::fs::read_to_string(pub_file(&self.root)) {
            Ok(text) => text,
            Err(e) if is_not_found(&e) => {
                return Reply::text(400, "this registry has no verifying key to check against");
            }
            Err(e) => return Reply::text(500, format!("read key: {e}")),
        };
        let sig_hex = match std::str::from_utf8(body) {
            Ok(text) => text,
            Err(_) => return Reply::text(400, "signature is not utf-8 hex"),
        };
        match verify_detached(&pub_hex, sig_hex, &artifact_bytes) {
            Ok(raw_sig) => {
                let canonical = format!("{}\n", hex_encode(&raw_sig));
                match write_via_tmp(&sig_file(&self.root, id), canonical.as_bytes()) {
                    Ok(()) => Reply::text(201, format!("signature for `{id}` accepted")),
                    Err(e) => Reply::text(500, format!("write signature: {e}")),
                }
            }
            Err(detail) => Reply::text(400, format!("signature rejected: {detail}")),
        }
    }
}

/// The single 404 the protocol hands to everything outside its route table.
fn not_found() -> Reply {
    Reply::text(404, "not found")
}

/// Bind `addr` and return the server plus the port it actually bound (so a
/// caller that asked for port 0 can learn the ephemeral port — the loopback
/// integration test does exactly this).
pub fn bind(addr: &str) -> Result<(tiny_http::Server, u16), RemoteError> {
    let server = tiny_http::Server::http(addr).map_err(|e| RemoteError::Bind {
        addr: addr.to_owned(),
        detail: e.to_string(),
    })?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|socket| socket.port())
        .ok_or_else(|| RemoteError::Bind {
            addr: addr.to_owned(),
            detail: "bound socket has no IP port".to_owned(),
        })?;
    Ok((server, port))
}

/// The socket shell: a blocking, sequential accept loop that parses each
/// request into `(Verb, path, body)`, answers it with [`RegistryHost::handle`],
/// and writes the reply back with one log line. Sequential is the v0 choice
/// (correctness first), matching `auto-serve`; it returns only on a
/// socket-level accept failure.
pub fn serve(server: tiny_http::Server, host: RegistryHost) -> Result<(), RemoteError> {
    loop {
        let mut request = match server.recv() {
            Ok(request) => request,
            Err(e) => {
                return Err(RemoteError::Loop {
                    detail: e.to_string(),
                });
            }
        };
        let label = request.method().to_string();
        let verb = map_verb(request.method());
        let path = request.url().to_owned();

        let mut body = Vec::new();
        if let Err(e) = request.as_reader().read_to_end(&mut body) {
            let reply = Reply::text(400, format!("could not read request body: {e}"));
            eprintln!("auto registry serve: {label} {path} -> {}", reply.status);
            respond(request, reply);
            continue;
        }

        let reply = host.handle(verb, &path, &body);
        eprintln!("auto registry serve: {label} {path} -> {}", reply.status);
        respond(request, reply);
    }
}

/// Open the registry at `root`, bind `addr`, and serve until a socket-level
/// failure. The convenience entry the CLI wires `registry serve` to.
pub fn serve_addr(root: &Path, addr: &str) -> Result<(), RemoteError> {
    let host = RegistryHost::open(root)?;
    let (server, port) = bind(addr)?;
    eprintln!(
        "auto registry serve: listening on {addr} (port {port}); registry {}; loopback dev transport, NO auth, NO TLS (ADR-0022)",
        root.display()
    );
    serve(server, host)
}

fn map_verb(method: &tiny_http::Method) -> Verb {
    match method {
        tiny_http::Method::Get => Verb::Get,
        tiny_http::Method::Put => Verb::Put,
        _ => Verb::Other,
    }
}

/// Serialize one [`Reply`] onto the socket. A send failure (peer hung up) is
/// logged, not fatal.
fn respond(request: tiny_http::Request, reply: Reply) {
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], reply.content_type.as_bytes())
        .expect("a static, valid Content-Type header");
    let response = tiny_http::Response::from_data(reply.body)
        .with_status_code(reply.status)
        .with_header(header);
    if let Err(e) = request.respond(response) {
        eprintln!("auto registry serve: could not send response: {e}");
    }
}

// ---------------------------------------------------------------------------
// client side: push / pull
// ---------------------------------------------------------------------------

/// Outcome of a [`push`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSummary {
    pub id: String,
    /// `true` when the server stored the bytes for the first time (201);
    /// `false` when they were already present (200 idempotent).
    pub created: bool,
    /// `true` when a detached signature was pushed and accepted (201).
    pub signed: bool,
}

/// Outcome of a [`pull`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullSummary {
    pub id: String,
    /// `true` when the artifact carried a signature that verified end-to-end.
    pub signed: bool,
    /// The local store re-verified the written artifact through
    /// [`Registry::get`] (content id recomputed; signature checked). Always
    /// `true` in a returned summary — a failure is an `Err`.
    pub verified: bool,
}

/// Push artifact `id` from the local registry at `root` to the remote at
/// `base_url`.
///
/// The local copy is fetched through [`Registry::get`] first, so a locally
/// corrupt or tampered artifact is refused *before* it reaches the wire
/// (verification at the sending end). The verified bytes are `PUT`; if the
/// local artifact is signed, its detached signature is `PUT` too and must be
/// accepted by the server (which re-verifies it against its own key).
pub fn push(base_url: &str, root: &Path, id: &str) -> Result<PushSummary, RemoteError> {
    if !is_content_id(id) {
        return Err(RemoteError::Invalid {
            detail: format!("`{id}` is not a 64-hex content id"),
        });
    }
    let base = base_url.trim_end_matches('/');
    let registry = Registry::open(root)?;

    // sending-end verification: get() re-parses, recomputes the id, and
    // verifies any signature before we read the bytes back
    let tmp = temp_path("push");
    let verified = registry.get(id, &tmp).map_err(|e| match e {
        RegistryError::NotFound { .. } => RemoteError::NotFound { id: id.to_owned() },
        other => RemoteError::Registry(other),
    });
    let bytes = verified.and_then(|outcome| {
        let read = std::fs::read(&tmp).map_err(RemoteError::Io);
        read.map(|bytes| (bytes, outcome.signature.is_some()))
    });
    let _ = std::fs::remove_file(&tmp);
    let (bytes, signed_locally) = bytes?;

    let agent = build_agent();

    // PUT the artifact bytes: 200 (idempotent) or 201 (created) is success
    let (status, detail) = put_bytes(&agent, &format!("{base}{API}/artifacts/{id}"), &bytes)?;
    let created = match status {
        201 => true,
        200 => false,
        other => {
            return Err(RemoteError::Status {
                code: other,
                detail,
            });
        }
    };

    // PUT the signature if we have one locally; the server accepts it only if
    // it verifies against the server key (shared trust root)
    if signed_locally {
        let sig_bytes = std::fs::read(sig_file(root, id))?;
        let (sig_status, sig_detail) = put_bytes(
            &agent,
            &format!("{base}{API}/artifacts/{id}/signature"),
            &sig_bytes,
        )?;
        if sig_status != 201 {
            return Err(RemoteError::Status {
                code: sig_status,
                detail: format!(
                    "artifact `{id}` stored, but its signature was refused: {sig_detail}"
                ),
            });
        }
    }

    Ok(PushSummary {
        id: id.to_owned(),
        created,
        signed: signed_locally,
    })
}

/// Pull artifact `id` from the remote at `base_url` into the local registry at
/// `root`.
///
/// Every check runs before anything is written: the fetched bytes must hash to
/// the requested id (receiving-end tamper check); a fetched signature must
/// verify against the remote's key (`GET /v0/key`); and the local trust root
/// must not already hold a *different* verifying key. Only then is the artifact
/// stored through [`Registry::add`] (the local invariants hold), the verified
/// signature written beside it, and the whole thing re-verified through
/// [`Registry::get`].
pub fn pull(base_url: &str, root: &Path, id: &str) -> Result<PullSummary, RemoteError> {
    if !is_content_id(id) {
        return Err(RemoteError::Invalid {
            detail: format!("`{id}` is not a 64-hex content id"),
        });
    }
    let base = base_url.trim_end_matches('/');
    let agent = build_agent();
    let registry = Registry::open(root)?;

    // 1. fetch bytes
    let (status, bytes) = get_bytes(&agent, &format!("{base}{API}/artifacts/{id}"))?;
    match status {
        200 => {}
        404 => return Err(RemoteError::NotFound { id: id.to_owned() }),
        other => {
            return Err(RemoteError::Status {
                code: other,
                detail: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
    }

    // 2. receiving-end verification: the bytes must hash to the id we asked for
    let artifact = Artifact::from_bytes(&bytes).map_err(|e| RemoteError::Invalid {
        detail: format!("pulled bytes are not a valid artifact: {e}"),
    })?;
    let actual = artifact.id();
    if actual != id {
        return Err(RemoteError::DigestMismatch {
            expected: id.to_owned(),
            actual,
        });
    }

    // 3. fetch the signature (may be absent)
    let (sig_status, sig_body) =
        get_bytes(&agent, &format!("{base}{API}/artifacts/{id}/signature"))?;
    let sig_hex = match sig_status {
        200 => Some(
            String::from_utf8(sig_body).map_err(|_| RemoteError::Invalid {
                detail: "remote signature is not utf-8".to_owned(),
            })?,
        ),
        404 => None,
        other => {
            return Err(RemoteError::Status {
                code: other,
                detail: "unexpected status fetching signature".to_owned(),
            });
        }
    };

    // 4. if signed, fetch the key and verify BEFORE writing anything
    let mut remote_key: Option<Vec<u8>> = None;
    if let Some(ref sig_hex) = sig_hex {
        let (key_status, key_body) = get_bytes(&agent, &format!("{base}{API}/key"))?;
        let key_hex = match key_status {
            200 => String::from_utf8(key_body.clone()).map_err(|_| RemoteError::Invalid {
                detail: "remote key is not utf-8".to_owned(),
            })?,
            404 => {
                return Err(RemoteError::BadSignature {
                    id: id.to_owned(),
                    detail: "artifact is signed but the remote exposes no verifying key".to_owned(),
                });
            }
            other => {
                return Err(RemoteError::Status {
                    code: other,
                    detail: "unexpected status fetching key".to_owned(),
                });
            }
        };
        verify_detached(&key_hex, sig_hex, &bytes).map_err(|detail| RemoteError::BadSignature {
            id: id.to_owned(),
            detail,
        })?;
        remote_key = Some(key_body);
    }

    // 5. reconcile the local verifying key with the remote's (trust root):
    //    install it if absent, accept it if identical, refuse on conflict
    if let Some(ref key_body) = remote_key {
        reconcile_pub_key(root, key_body)?;
    }

    // 6. store the artifact through the public publish path (content-addressed,
    //    validated), then write the verified signature beside it
    let tmp = temp_path("pull");
    std::fs::write(&tmp, &bytes)?;
    let added = registry.add(&tmp, false);
    let _ = std::fs::remove_file(&tmp);
    added.map_err(RemoteError::Registry)?;

    if let Some(ref sig_hex) = sig_hex {
        let raw = hex_decode(sig_hex.trim()).map_err(|detail| RemoteError::Invalid {
            detail: format!("remote signature hex: {detail}"),
        })?;
        let canonical = format!("{}\n", hex_encode(&raw));
        write_via_tmp(&sig_file(root, id), canonical.as_bytes())?;
    }

    // 7. both-ends confirmation: the local store must now verify what we wrote
    let confirm = temp_path("pull-confirm");
    let outcome = registry.get(id, &confirm);
    let _ = std::fs::remove_file(&confirm);
    let outcome = outcome.map_err(RemoteError::Registry)?;

    Ok(PullSummary {
        id: id.to_owned(),
        signed: sig_hex.is_some(),
        verified: outcome.verified_content,
    })
}

/// Install the remote verifying key as the local `keys/auto.pub` when the local
/// root has none; accept silently when it already matches; refuse on a genuine
/// conflict (a different local trust root must not be overwritten by a pull).
fn reconcile_pub_key(root: &Path, remote_key_bytes: &[u8]) -> Result<(), RemoteError> {
    let remote_hex = String::from_utf8_lossy(remote_key_bytes);
    let remote_raw = hex_decode(remote_hex.trim()).map_err(|detail| RemoteError::Invalid {
        detail: format!("remote key hex: {detail}"),
    })?;
    let path = pub_file(root);
    match std::fs::read_to_string(&path) {
        Ok(local_hex) => {
            let local_raw =
                hex_decode(local_hex.trim()).map_err(|detail| RemoteError::Invalid {
                    detail: format!("local key hex: {detail}"),
                })?;
            if local_raw != remote_raw {
                return Err(RemoteError::KeyConflict {
                    detail: format!(
                        "local registry already trusts a different key than {}; refusing to overwrite the trust root",
                        path.display()
                    ),
                });
            }
            Ok(())
        }
        Err(e) if is_not_found(&e) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            // write canonically, exactly as `Registry` stores its own auto.pub
            let canonical = format!("{}\n", hex_encode(&remote_raw));
            write_via_tmp(&path, canonical.as_bytes())?;
            Ok(())
        }
        Err(e) => Err(RemoteError::Io(e)),
    }
}

/// One blocking `ureq` agent. `http_status_as_error(false)` so a 4xx/9xx status
/// is read as a normal response (we route on the code), matching the frontier
/// client and the recording proxy.
fn build_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(CLIENT_TIMEOUT))
        .build()
        .new_agent()
}

/// `GET url` → `(status, body bytes)`. A transport failure is [`RemoteError::Http`].
fn get_bytes(agent: &ureq::Agent, url: &str) -> Result<(u16, Vec<u8>), RemoteError> {
    let mut response = agent.get(url).call().map_err(|e| RemoteError::Http {
        detail: e.to_string(),
    })?;
    let status = response.status().as_u16();
    let bytes = response
        .body_mut()
        .read_to_vec()
        .map_err(|e| RemoteError::Http {
            detail: e.to_string(),
        })?;
    Ok((status, bytes))
}

/// `PUT url` with an octet-stream body → `(status, body text)`.
fn put_bytes(agent: &ureq::Agent, url: &str, body: &[u8]) -> Result<(u16, String), RemoteError> {
    let mut response = agent
        .put(url)
        .header("content-type", OCTET)
        .send(body)
        .map_err(|e| RemoteError::Http {
            detail: e.to_string(),
        })?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap_or_default();
    Ok((status, text))
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Verify a detached signature (hex) against a verifying key (hex) over `msg`.
/// Returns the raw signature bytes on success so the caller can re-encode them
/// canonically. `Err` is an honest description of the first failing step.
fn verify_detached(pub_hex: &str, sig_hex: &str, msg: &[u8]) -> Result<Vec<u8>, String> {
    let pub_raw = hex_decode(pub_hex.trim())?;
    let pub_arr: [u8; PUBLIC_KEY_LENGTH] = pub_raw
        .try_into()
        .map_err(|_| "verifying key is not 32 bytes".to_string())?;
    let key = VerifyingKey::from_bytes(&pub_arr).map_err(|e| format!("bad verifying key: {e}"))?;
    let sig_raw = hex_decode(sig_hex.trim())?;
    let sig = Signature::from_slice(&sig_raw).map_err(|e| format!("malformed signature: {e}"))?;
    // verify_strict also rejects small-order components, matching Registry::get
    key.verify_strict(msg, &sig)
        .map_err(|e| format!("verification failed: {e}"))?;
    Ok(sig_raw)
}

fn artifact_file(root: &Path, id: &str) -> PathBuf {
    root.join(ARTIFACTS_DIR)
        .join(format!("{id}.{ARTIFACT_EXT}"))
}

fn sig_file(root: &Path, id: &str) -> PathBuf {
    root.join(ARTIFACTS_DIR).join(format!("{id}.sig"))
}

fn pub_file(root: &Path) -> PathBuf {
    root.join(KEYS_DIR).join(PUB_FILE)
}

fn is_not_found(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

/// A process-unique scratch path under the temp dir for the temp-file dance
/// [`Registry::get`] / [`Registry::add`] require (they take file paths). Mirrors
/// the pattern in `auto-serve`.
fn temp_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "auto-registry-remote-{}-{tag}-{unique}.cbin",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use auto_backend::{
        Artifact, MANIFEST_ENTRY, MANIFEST_VERSION, MODULE_ENTRY, Manifest, Measured, Provenance,
    };

    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            manifest_version: MANIFEST_VERSION,
            task: "toy-agent".into(),
            scope_kind: "model_call".into(),
            scope_name: "fake-frontier".into(),
            interface_input: "json".into(),
            interface_output: "text".into(),
            capabilities: vec![],
            contract_id: "c".repeat(8),
            eval_run_ids: vec!["run-1".into()],
            provenance: Provenance {
                trace_ids: vec!["0".repeat(32)],
                reference: "test reference".into(),
                observations: 2,
            },
            measured: Measured {
                compiled_latency_ms_p50: 1,
                compiled_latency_ms_p95: 2,
                compiled_latency_ms_max: 3,
                reference_recorded_latency_ms_p95: 40,
            },
            notes: String::new(),
        }
    }

    /// Container bytes; `module` distinguishes the content id so several
    /// artifacts can coexist in one store.
    fn artifact_bytes(module: &[u8]) -> Vec<u8> {
        let mut entries = BTreeMap::new();
        entries.insert(
            MANIFEST_ENTRY.to_owned(),
            manifest().canonical_json().into_bytes(),
        );
        entries.insert(MODULE_ENTRY.to_owned(), module.to_vec());
        Artifact::new(entries).to_bytes()
    }

    fn content_id(bytes: &[u8]) -> String {
        Artifact::from_bytes(bytes).expect("valid artifact").id()
    }

    /// Add `bytes` to `host`'s registry through a source file; returns the id.
    fn add(host: &RegistryHost, dir: &Path, name: &str, bytes: &[u8], sign: bool) -> String {
        let src = dir.join(name);
        std::fs::write(&src, bytes).expect("write source");
        host.registry.add(&src, sign).expect("add").id
    }

    /// A host over a fresh registry root inside `dir`.
    fn host(dir: &Path) -> RegistryHost {
        RegistryHost::open(&dir.join("registry")).expect("open host")
    }

    fn get(host: &RegistryHost, path: &str) -> Reply {
        host.handle(Verb::Get, path, &[])
    }

    fn put(host: &RegistryHost, path: &str, body: &[u8]) -> Reply {
        host.handle(Verb::Put, path, body)
    }

    #[test]
    fn list_is_sorted_ids_one_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let id_a = add(
            &host,
            dir.path(),
            "a.cbin",
            &artifact_bytes(b"module A"),
            false,
        );
        let id_b = add(
            &host,
            dir.path(),
            "b.cbin",
            &artifact_bytes(b"module B"),
            false,
        );

        let reply = get(&host, "/v0/artifacts");
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, TEXT);
        let listed: Vec<&str> = std::str::from_utf8(&reply.body).unwrap().lines().collect();
        let mut expected = vec![id_a.as_str(), id_b.as_str()];
        expected.sort_unstable();
        assert_eq!(listed, expected);
    }

    #[test]
    fn list_empty_registry_is_empty_200() {
        let dir = tempfile::tempdir().unwrap();
        let reply = get(&host(dir.path()), "/v0/artifacts");
        assert_eq!(reply.status, 200);
        assert!(reply.body.is_empty());
    }

    #[test]
    fn get_artifact_returns_verified_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let bytes = artifact_bytes(b"module");
        let id = add(&host, dir.path(), "a.cbin", &bytes, false);

        let reply = get(&host, &format!("/v0/artifacts/{id}"));
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, OCTET);
        assert_eq!(reply.body, bytes);
    }

    #[test]
    fn get_unknown_artifact_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let reply = get(
            &host(dir.path()),
            &format!("/v0/artifacts/{}", "0".repeat(64)),
        );
        assert_eq!(reply.status, 404);
    }

    #[test]
    fn get_malformed_id_is_400() {
        let dir = tempfile::tempdir().unwrap();
        let reply = get(&host(dir.path()), "/v0/artifacts/not-a-content-id");
        assert_eq!(reply.status, 400);
    }

    #[test]
    fn get_tampered_artifact_is_500_never_silent_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let id = add(
            &host,
            dir.path(),
            "a.cbin",
            &artifact_bytes(b"module"),
            false,
        );
        // flip the last byte of the stored file: still a parseable container,
        // but the content id no longer matches the filename
        let stored = artifact_file(&host.root, &id);
        let mut raw = std::fs::read(&stored).unwrap();
        *raw.last_mut().unwrap() ^= 0xff;
        std::fs::write(&stored, &raw).unwrap();

        let reply = get(&host, &format!("/v0/artifacts/{id}"));
        assert_eq!(reply.status, 500);
    }

    #[test]
    fn get_key_present_and_absent() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        // absent
        assert_eq!(get(&host, "/v0/key").status, 404);
        // present
        host.registry.keygen().expect("keygen");
        let reply = get(&host, "/v0/key");
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, OCTET);
        // the served bytes are exactly what the store keeps in auto.pub
        assert_eq!(reply.body, std::fs::read(pub_file(&host.root)).unwrap());
    }

    #[test]
    fn get_signature_present_and_absent() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        host.registry.keygen().expect("keygen");
        let id = add(
            &host,
            dir.path(),
            "a.cbin",
            &artifact_bytes(b"module"),
            true,
        );

        let signed = get(&host, &format!("/v0/artifacts/{id}/signature"));
        assert_eq!(signed.status, 200);
        assert_eq!(signed.content_type, OCTET);
        assert_eq!(
            signed.body,
            std::fs::read(sig_file(&host.root, &id)).unwrap()
        );

        // an unsigned artifact has no signature
        let id2 = add(
            &host,
            dir.path(),
            "b.cbin",
            &artifact_bytes(b"module B"),
            false,
        );
        assert_eq!(
            get(&host, &format!("/v0/artifacts/{id2}/signature")).status,
            404
        );
    }

    #[test]
    fn put_artifact_creates_then_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let bytes = artifact_bytes(b"module");
        let id = content_id(&bytes);

        let created = put(&host, &format!("/v0/artifacts/{id}"), &bytes);
        assert_eq!(created.status, 201);
        // it is now listed and served
        assert_eq!(get(&host, &format!("/v0/artifacts/{id}")).body, bytes);
        // same bytes again → idempotent 200
        let again = put(&host, &format!("/v0/artifacts/{id}"), &bytes);
        assert_eq!(again.status, 200);
    }

    #[test]
    fn put_artifact_digest_mismatch_is_400_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let bytes = artifact_bytes(b"module");
        // a valid but wrong path id
        let wrong = "a".repeat(64);
        let reply = put(&host, &format!("/v0/artifacts/{wrong}"), &bytes);
        assert_eq!(reply.status, 400);
        assert!(!artifact_file(&host.root, &wrong).exists());
        // and nothing was stored under the true id either
        assert!(get(&host, "/v0/artifacts").body.is_empty());
    }

    #[test]
    fn put_non_artifact_body_is_400() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let reply = put(
            &host,
            &format!("/v0/artifacts/{}", "b".repeat(64)),
            b"not a container",
        );
        assert_eq!(reply.status, 400);
    }

    #[test]
    fn put_artifact_over_tampered_store_bytes_is_409() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let bytes = artifact_bytes(b"module");
        let id = add(&host, dir.path(), "a.cbin", &bytes, false);
        // tamper the stored file with DIFFERENT valid bytes filed under `id`
        let other = artifact_bytes(b"different module");
        std::fs::write(artifact_file(&host.root, &id), &other).unwrap();
        // re-PUT the ORIGINAL bytes (which hash to `id`): the store holds
        // different bytes under that id → 409 tamper evidence
        let reply = put(&host, &format!("/v0/artifacts/{id}"), &bytes);
        assert_eq!(reply.status, 409);
    }

    #[test]
    fn put_signature_accepts_a_valid_signature() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        host.registry.keygen().expect("keygen");
        // add signed so the store computes a real signature, then capture and
        // remove it so the PUT genuinely creates it
        let id = add(
            &host,
            dir.path(),
            "a.cbin",
            &artifact_bytes(b"module"),
            true,
        );
        let sig_path = sig_file(&host.root, &id);
        let sig_bytes = std::fs::read(&sig_path).unwrap();
        std::fs::remove_file(&sig_path).unwrap();

        let reply = put(&host, &format!("/v0/artifacts/{id}/signature"), &sig_bytes);
        assert_eq!(reply.status, 201);
        // the signature is back and served, and the artifact still verifies
        assert_eq!(
            get(&host, &format!("/v0/artifacts/{id}/signature")).status,
            200
        );
        assert_eq!(get(&host, &format!("/v0/artifacts/{id}")).status, 200);
    }

    #[test]
    fn put_signature_rejects_a_bad_signature() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        host.registry.keygen().expect("keygen");
        let id = add(
            &host,
            dir.path(),
            "a.cbin",
            &artifact_bytes(b"module"),
            false,
        );

        // valid hex, wrong signature
        let bogus = "0".repeat(128);
        assert_eq!(
            put(
                &host,
                &format!("/v0/artifacts/{id}/signature"),
                bogus.as_bytes()
            )
            .status,
            400
        );
        // not hex at all
        assert_eq!(
            put(&host, &format!("/v0/artifacts/{id}/signature"), b"not hex").status,
            400
        );
        // nothing was written
        assert!(!sig_file(&host.root, &id).exists());
    }

    #[test]
    fn put_signature_without_artifact_is_400() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        host.registry.keygen().expect("keygen");
        let reply = put(
            &host,
            &format!("/v0/artifacts/{}/signature", "c".repeat(64)),
            &"0".repeat(128).into_bytes(),
        );
        assert_eq!(reply.status, 400);
    }

    #[test]
    fn put_signature_without_registry_key_is_400() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        // no keygen: the registry has no verifying key to check against
        let id = add(
            &host,
            dir.path(),
            "a.cbin",
            &artifact_bytes(b"module"),
            false,
        );
        let reply = put(
            &host,
            &format!("/v0/artifacts/{id}/signature"),
            &"0".repeat(128).into_bytes(),
        );
        assert_eq!(reply.status, 400);
    }

    #[test]
    fn unknown_route_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        assert_eq!(get(&host, "/v0/nope").status, 404);
        assert_eq!(get(&host, "/").status, 404);
        assert_eq!(get(&host, "/v0/artifacts/extra/segments/here").status, 400);
    }

    #[test]
    fn non_get_put_method_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        // a POST-shaped request (Verb::Other) on any real route is not found
        assert_eq!(host.handle(Verb::Other, "/v0/artifacts", &[]).status, 404);
        let id = "d".repeat(64);
        assert_eq!(
            host.handle(Verb::Other, &format!("/v0/artifacts/{id}"), &[])
                .status,
            404
        );
        // PUT is not a listing/key verb
        assert_eq!(put(&host, "/v0/artifacts", &[]).status, 404);
        assert_eq!(put(&host, "/v0/key", &[]).status, 404);
    }

    #[test]
    fn query_string_is_ignored_by_the_router() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        assert_eq!(get(&host, "/v0/artifacts?probe=1").status, 200);
    }

    #[test]
    fn verify_detached_roundtrips_a_real_signature() {
        // build a signed artifact through the store, then verify its .sig with
        // the same helper the PUT path uses — a positive check on verify_detached
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        host.registry.keygen().expect("keygen");
        let bytes = artifact_bytes(b"module");
        let id = add(&host, dir.path(), "a.cbin", &bytes, true);
        let pub_hex = std::fs::read_to_string(pub_file(&host.root)).unwrap();
        let sig_hex = std::fs::read_to_string(sig_file(&host.root, &id)).unwrap();
        assert!(verify_detached(&pub_hex, &sig_hex, &bytes).is_ok());
        // a one-byte change to the message breaks it
        let mut tampered = bytes.clone();
        *tampered.last_mut().unwrap() ^= 0xff;
        assert!(verify_detached(&pub_hex, &sig_hex, &tampered).is_err());
    }
}
