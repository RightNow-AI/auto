//! One loopback integration test for the remote transport (ADR-0022): a real
//! `tiny_http` server on `127.0.0.1:0`, a push from one registry root and a
//! pull into a second, artifact + signature verified end to end, then a
//! tampered server file proving the pull refuses.
//!
//! The wire cannot be tampered from inside `ureq`, so the test tampers the
//! artifact AT REST on the server: the server's own [`auto_registry::Registry`]
//! recomputes the content id before serving and refuses (500), so corrupt bytes
//! never reach the client. No external network — loopback only.

use std::collections::BTreeMap;
use std::path::Path;

use auto_backend::{
    Artifact, MANIFEST_ENTRY, MANIFEST_VERSION, MODULE_ENTRY, Manifest, Measured, Provenance,
};
use auto_registry::Registry;
use auto_registry::remote::{RegistryHost, RemoteError, bind, pull, push, serve};

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

fn artifact_bytes() -> Vec<u8> {
    let mut entries = BTreeMap::new();
    entries.insert(
        MANIFEST_ENTRY.to_owned(),
        manifest().canonical_json().into_bytes(),
    );
    entries.insert(
        MODULE_ENTRY.to_owned(),
        b"not wasm; the registry never executes modules".to_vec(),
    );
    Artifact::new(entries).to_bytes()
}

fn artifact_file(root: &Path, id: &str) -> std::path::PathBuf {
    root.join("artifacts").join(format!("{id}.cbin"))
}

#[test]
fn push_then_pull_over_loopback_then_tamper_refuses() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let local = scratch.path().join("local");
    let server_root = scratch.path().join("server");
    let fresh = scratch.path().join("fresh");
    let fresh_after_tamper = scratch.path().join("fresh2");

    // --- local registry: keygen + add a SIGNED artifact ---
    let local_reg = Registry::open(&local).expect("open local");
    local_reg.keygen().expect("keygen");
    let bytes = artifact_bytes();
    let src = scratch.path().join("task.cbin");
    std::fs::write(&src, &bytes).expect("write source");
    let id = local_reg.add(&src, true).expect("add --sign").id;

    // --- server registry: trust the local key (verify pushed signatures and
    // expose it via GET /v0/key). A real deployment provisions the org key; the
    // test copies the local public key. ---
    let server_host = RegistryHost::open(&server_root).expect("open server host");
    std::fs::copy(
        local.join("keys").join("auto.pub"),
        server_root.join("keys").join("auto.pub"),
    )
    .expect("share verifying key");

    // --- bind an ephemeral port, then serve on a background thread ---
    let (server, port) = bind("127.0.0.1:0").expect("bind loopback");
    std::thread::spawn(move || {
        // returns only on a socket-level failure; abandoned at test exit
        let _ = serve(server, server_host);
    });
    let base = format!("http://127.0.0.1:{port}");

    // --- push local -> server: creates the artifact and its signature ---
    let pushed = push(&base, &local, &id).expect("push");
    assert_eq!(pushed.id, id);
    assert!(pushed.created, "first push stores the bytes (201)");
    assert!(
        pushed.signed,
        "the signed artifact's signature was accepted"
    );

    // idempotent re-push: same bytes are a no-op (200)
    let repushed = push(&base, &local, &id).expect("re-push");
    assert!(!repushed.created, "identical bytes re-push is idempotent");

    // --- pull server -> fresh root: verified end to end ---
    let pulled = pull(&base, &fresh, &id).expect("pull");
    assert_eq!(pulled.id, id);
    assert!(pulled.signed, "the pulled artifact is signed");
    assert!(
        pulled.verified,
        "the local store re-verified the pulled copy"
    );

    // the pulled copy verifies through the ordinary registry path and is
    // byte-identical to the original
    let fresh_reg = Registry::open(&fresh).expect("open fresh");
    let out = scratch.path().join("out.cbin");
    let got = fresh_reg.get(&id, &out).expect("get pulled");
    assert_eq!(
        got.signature,
        Some(true),
        "pulled signature verifies locally"
    );
    assert_eq!(
        std::fs::read(&out).unwrap(),
        bytes,
        "byte-identical roundtrip"
    );

    // --- a pull into a root that already trusts a DIFFERENT key refuses,
    // rather than overwrite a local trust root ---
    let conflicting = scratch.path().join("conflicting");
    Registry::open(&conflicting)
        .expect("open conflicting")
        .keygen()
        .expect("a different keypair");
    let clash = pull(&base, &conflicting, &id);
    assert!(
        matches!(clash, Err(RemoteError::KeyConflict { .. })),
        "pull must refuse a conflicting local trust root; got {clash:?}"
    );
    assert!(
        !artifact_file(&conflicting, &id).exists(),
        "a refused pull writes no artifact"
    );

    // --- tamper the artifact AT REST on the server, then pull must refuse ---
    let stored = artifact_file(&server_root, &id);
    let mut raw = std::fs::read(&stored).expect("read server artifact");
    *raw.last_mut().unwrap() ^= 0xff; // still a parseable container, wrong id
    std::fs::write(&stored, &raw).expect("tamper server artifact");

    let refused = pull(&base, &fresh_after_tamper, &id);
    assert!(
        matches!(refused, Err(RemoteError::Status { code: 500, .. })),
        "server recomputes the id and refuses to serve tampered bytes; got {refused:?}"
    );
    // nothing was written into the second fresh root
    assert!(
        !artifact_file(&fresh_after_tamper, &id).exists(),
        "a refused pull writes no artifact"
    );
}
