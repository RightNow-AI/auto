//! Auto registry — S7 slice: a local content-addressed artifact store with
//! detached ed25519 signing.
//!
//! Layout under the registry root:
//!
//! ```text
//! <root>/artifacts/<id>.cbin   artifact bytes, verbatim; <id> = sha-256 hex of the bytes
//! <root>/artifacts/<id>.sig    detached ed25519 signature over the raw bytes, hex
//! <root>/keys/auto.key         signing seed, 32 bytes hex — stored in the clear
//! <root>/keys/auto.pub         verifying key, 32 bytes hex
//! ```
//!
//! Every handout is checked: [`Registry::get`] re-parses the stored bytes,
//! recomputes the content id, and verifies the signature when one exists —
//! a mismatch is an error, never a warning. Signatures are detached and sign
//! the raw container bytes, so signing never changes an artifact's id.
//!
//! Not here (honest bounds): remote registries, sigstore, key passphrases or
//! rotation — one cleartext local keypair per registry root, no protection
//! against an attacker who can write to that root's `keys/` directory.
//! Recorded targets: CLAUDE.md S7, spec/adr/open-questions.md.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use auto_backend::Artifact;
use ed25519_dalek::{
    PUBLIC_KEY_LENGTH, SECRET_KEY_LENGTH, Signature, Signer, SigningKey, VerifyingKey,
};

pub mod remote;
pub use remote::{PullSummary, PushSummary, RemoteError};

const ARTIFACTS_DIR: &str = "artifacts";
const KEYS_DIR: &str = "keys";
const KEY_FILE: &str = "auto.key";
const PUB_FILE: &str = "auto.pub";
const ARTIFACT_EXT: &str = "cbin";

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The bytes are not a valid `.cbin` (container or manifest error text).
    #[error("invalid artifact: {0}")]
    InvalidArtifact(String),
    #[error("artifact `{id}` not found in the registry")]
    NotFound { id: String },
    /// Stored bytes do not hash to the id they are filed under — tamper
    /// evidence. `actual` is the recomputed id, or a description when the
    /// stored bytes no longer even parse.
    #[error(
        "content id mismatch (tamper evidence): expected `{expected}`, stored bytes give `{actual}`"
    )]
    IdMismatch { expected: String, actual: String },
    #[error("key already exists at `{path}`; refusing to overwrite")]
    KeyExists { path: String },
    /// A signing key (or, for verification, the public key) is required but
    /// absent.
    #[error("no key at `{path}`; run keygen first")]
    NoKey { path: String },
    /// Key file exists but its content is not a usable key.
    #[error("bad key material: {0}")]
    BadKey(String),
    #[error("bad signature for `{id}` (tamper evidence): {detail}")]
    BadSignature { id: String, detail: String },
    /// Never constructed by this crate — `get` reports an unsigned artifact
    /// as `signature: None`. Reserved for callers that require a signature
    /// (e.g. a cli `--require-signed` flow).
    #[error("artifact `{id}` is not signed")]
    NoSignature { id: String },
}

/// Result of [`Registry::add`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddOutcome {
    pub id: String,
    pub signed: bool,
    pub stored_at: PathBuf,
}

/// Result of [`Registry::get`]. Only returned when every check passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetOutcome {
    /// Recomputed content id matched the requested id. Always `true` in a
    /// returned outcome — a mismatch is `Err(IdMismatch)`; the field exists
    /// so callers can report the check honestly rather than assume it.
    pub verified_content: bool,
    /// `Some(true)`: a signature existed and verified against `auto.pub`.
    /// `None`: no signature file. A failing signature is `Err(BadSignature)`,
    /// never `Some(false)` — a bad-signature artifact is not handed out.
    pub signature: Option<bool>,
}

/// One row of [`Registry::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The id the file is stored under (filename stem).
    pub id: String,
    /// Manifest `task`; empty when `problem` is set.
    pub task: String,
    /// Manifest scope as `kind(name)`, e.g. `model_call(fake-frontier)`;
    /// empty when `problem` is set.
    pub scope: String,
    /// Number of PASS eval runs cited by the manifest; 0 when `problem` is set.
    pub eval_runs: usize,
    /// A `.sig` file exists next to the artifact.
    pub signed: bool,
    /// `Some(bool)`: signature checked against `keys/auto.pub` (over the raw
    /// stored bytes, so tampered content shows up as `Some(false)` too).
    /// `None`: unsigned, or signed but no `auto.pub` to check against.
    pub verified: Option<bool>,
    /// Why this entry could not be fully read (corrupt container, id
    /// mismatch, unreadable manifest). Per-entry so one bad file cannot hide
    /// the rest of the registry.
    pub problem: Option<String>,
}

/// A local registry rooted at one directory. See the module docs for layout.
#[derive(Debug, Clone)]
pub struct Registry {
    root: PathBuf,
}

impl Registry {
    /// Open a registry at `root`, creating `artifacts/` and `keys/` if needed.
    pub fn open(root: &Path) -> Result<Registry, RegistryError> {
        fs::create_dir_all(root.join(ARTIFACTS_DIR))?;
        fs::create_dir_all(root.join(KEYS_DIR))?;
        Ok(Registry {
            root: root.to_path_buf(),
        })
    }

    /// Generate the registry keypair. Refuses to overwrite existing key
    /// material. Returns the path of the written public key.
    ///
    /// The 32-byte seed comes from OS entropy ([`getrandom::fill`]) and is
    /// written hex-encoded, unencrypted (module docs: honest bounds).
    pub fn keygen(&self) -> Result<PathBuf, RegistryError> {
        let key_path = self.key_path();
        let pub_path = self.pub_path();
        for path in [&key_path, &pub_path] {
            if path.exists() {
                return Err(RegistryError::KeyExists {
                    path: path.display().to_string(),
                });
            }
        }
        let mut seed = [0u8; SECRET_KEY_LENGTH];
        getrandom::fill(&mut seed)
            .map_err(|e| std::io::Error::other(format!("entropy source failed: {e}")))?;
        let signing = SigningKey::from_bytes(&seed);
        fs::write(&key_path, format!("{}\n", hex_encode(&seed)))?;
        fs::write(
            &pub_path,
            format!("{}\n", hex_encode(&signing.verifying_key().to_bytes())),
        )?;
        Ok(pub_path)
    }

    /// Validate and store an artifact; optionally sign it.
    ///
    /// The store is content-addressed: re-adding identical bytes is a no-op.
    /// If a file already exists under the artifact's id with *different*
    /// bytes, that is tamper evidence and the add fails with `IdMismatch`.
    /// Signing signs the raw container bytes (detached `.sig`), loading the
    /// seed from `keys/auto.key` (`NoKey` when absent); nothing is stored if
    /// the key is missing.
    pub fn add(&self, artifact_path: &Path, sign: bool) -> Result<AddOutcome, RegistryError> {
        let bytes = fs::read(artifact_path)?;
        let artifact = Artifact::from_bytes(&bytes)
            .map_err(|e| RegistryError::InvalidArtifact(e.to_string()))?;
        // `list` reports from the manifest, so an unreadable manifest is
        // rejected at the door, not discovered later.
        artifact
            .manifest()
            .map_err(|e| RegistryError::InvalidArtifact(e.to_string()))?;
        let id = artifact.id();

        // Load the signing key before touching the store: a NoKey failure
        // must leave the registry unchanged.
        let signer = if sign {
            Some(self.load_signing_key()?)
        } else {
            None
        };

        let dest = self.artifact_path(&id);
        match fs::read(&dest) {
            Ok(existing) => {
                if existing != bytes {
                    let actual = match Artifact::from_bytes(&existing) {
                        Ok(stored) => stored.id(),
                        Err(e) => format!("unparseable stored bytes ({e})"),
                    };
                    return Err(RegistryError::IdMismatch {
                        expected: id,
                        actual,
                    });
                }
                // identical bytes already stored: content-addressed no-op
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write_via_tmp(&dest, &bytes)?;
            }
            Err(e) => return Err(e.into()),
        }

        if let Some(key) = signer {
            // Detached signature over the raw bytes; ed25519 signing is
            // deterministic, so re-signing the same bytes rewrites the same
            // signature. A torn write here fails verification loudly later.
            let sig = key.sign(&bytes);
            fs::write(
                self.sig_path(&id),
                format!("{}\n", hex_encode(&sig.to_bytes())),
            )?;
        }

        Ok(AddOutcome {
            id,
            signed: sign,
            stored_at: dest,
        })
    }

    /// List every `*.cbin` in the store, sorted by id.
    ///
    /// Per-file corruption (bad container, id mismatch, unreadable manifest)
    /// is surfaced in [`Entry::problem`] instead of failing the whole list,
    /// so one bad file cannot hide the rest. A corrupt `keys/auto.pub` *is* a
    /// whole-registry failure (`BadKey`): no signature could be checked.
    pub fn list(&self) -> Result<Vec<Entry>, RegistryError> {
        let verifying = self.load_verifying_key()?;
        let mut entries = Vec::new();
        for dirent in fs::read_dir(self.root.join(ARTIFACTS_DIR))? {
            let path = dirent?.path();
            if path.extension().and_then(|e| e.to_str()) != Some(ARTIFACT_EXT) {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            let bytes = fs::read(&path)?;
            let sig_path = self.sig_path(&id);
            let signed = sig_path.exists();
            let verified = match (&verifying, signed) {
                (Some(key), true) => Some(check_signature(key, &bytes, &sig_path).is_ok()),
                _ => None,
            };
            let (task, scope, eval_runs, problem) = describe(&id, &bytes);
            entries.push(Entry {
                id,
                task,
                scope,
                eval_runs,
                signed,
                verified,
                problem,
            });
        }
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(entries)
    }

    /// Copy artifact `id` to `out` — only after every check passes.
    ///
    /// Checks, in order: the stored bytes parse (`InvalidArtifact`), their
    /// recomputed content id equals the requested id (`IdMismatch`), and any
    /// detached signature verifies against `keys/auto.pub` (`BadSignature`;
    /// a signature with no public key to check it is `NoKey`). Unsigned
    /// artifacts pass with `signature: None`.
    pub fn get(&self, id: &str, out: &Path) -> Result<GetOutcome, RegistryError> {
        // Ids are 64 lowercase hex chars; anything else can never name a
        // stored artifact (and crafted ids must not traverse paths).
        if !is_content_id(id) {
            return Err(RegistryError::NotFound { id: id.to_owned() });
        }
        let path = self.artifact_path(id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(RegistryError::NotFound { id: id.to_owned() });
            }
            Err(e) => return Err(e.into()),
        };
        // `from_bytes` is strict-canonical, so re-serialization reproduces
        // the input bytes exactly and `id()` is the digest of the stored
        // bytes themselves.
        let artifact = Artifact::from_bytes(&bytes)
            .map_err(|e| RegistryError::InvalidArtifact(format!("stored artifact: {e}")))?;
        let actual = artifact.id();
        if actual != id {
            return Err(RegistryError::IdMismatch {
                expected: id.to_owned(),
                actual,
            });
        }
        let sig_path = self.sig_path(id);
        let signature = if sig_path.exists() {
            let Some(key) = self.load_verifying_key()? else {
                return Err(RegistryError::NoKey {
                    path: self.pub_path().display().to_string(),
                });
            };
            check_signature(&key, &bytes, &sig_path).map_err(|detail| {
                RegistryError::BadSignature {
                    id: id.to_owned(),
                    detail,
                }
            })?;
            Some(true)
        } else {
            None
        };
        fs::write(out, &bytes)?;
        Ok(GetOutcome {
            verified_content: true,
            signature,
        })
    }

    /// Every eval-run id pinned by a stored artifact's manifest — the set an
    /// eval-run GC must treat as protected.
    ///
    /// A manifest cites its `eval_run_ids` as the PASS evidence that gated the
    /// artifact's emit (the manifest is the trust layer); collecting one of
    /// those runs would sever an artifact from its provenance. This walks
    /// [`Registry::list`], re-reads each artifact, and re-verifies its content
    /// the same way [`Registry::get`] does — the recomputed content id must
    /// still equal the id it is filed under — before unioning the manifest's
    /// ids into the returned set.
    ///
    /// Corruption is loud, never a skip: an unparseable container, a content-id
    /// mismatch, or an unreadable manifest propagates as the usual
    /// [`RegistryError`]. A protection set that silently dropped a
    /// corrupt-but-real manifest's pins could let a GC delete a run that
    /// artifact still cites, so this refuses to yield a partial set. (Content
    /// integrity, not signature authenticity, is what makes a manifest's pinned
    /// ids trustworthy, so an unsigned or unverified-signature artifact still
    /// contributes its pins — a bad signature surfaces through
    /// [`Registry::get`], not here.)
    pub fn pinned_eval_runs(&self) -> Result<BTreeSet<String>, RegistryError> {
        let mut pinned = BTreeSet::new();
        for entry in self.list()? {
            let path = self.artifact_path(&entry.id);
            let bytes = fs::read(&path)?;
            let artifact = Artifact::from_bytes(&bytes)
                .map_err(|e| RegistryError::InvalidArtifact(format!("stored artifact: {e}")))?;
            let actual = artifact.id();
            if actual != entry.id {
                return Err(RegistryError::IdMismatch {
                    expected: entry.id,
                    actual,
                });
            }
            let manifest = artifact
                .manifest()
                .map_err(|e| RegistryError::InvalidArtifact(e.to_string()))?;
            pinned.extend(manifest.eval_run_ids);
        }
        Ok(pinned)
    }

    fn artifact_path(&self, id: &str) -> PathBuf {
        self.root.join(ARTIFACTS_DIR).join(format!("{id}.cbin"))
    }

    fn sig_path(&self, id: &str) -> PathBuf {
        self.root.join(ARTIFACTS_DIR).join(format!("{id}.sig"))
    }

    fn key_path(&self) -> PathBuf {
        self.root.join(KEYS_DIR).join(KEY_FILE)
    }

    fn pub_path(&self) -> PathBuf {
        self.root.join(KEYS_DIR).join(PUB_FILE)
    }

    fn load_signing_key(&self) -> Result<SigningKey, RegistryError> {
        let path = self.key_path();
        if !path.exists() {
            return Err(RegistryError::NoKey {
                path: path.display().to_string(),
            });
        }
        let text = fs::read_to_string(&path)?;
        let raw = hex_decode(text.trim())
            .map_err(|e| RegistryError::BadKey(format!("{}: {e}", path.display())))?;
        let seed: [u8; SECRET_KEY_LENGTH] = raw.try_into().map_err(|_| {
            RegistryError::BadKey(format!(
                "{}: seed must be {SECRET_KEY_LENGTH} bytes",
                path.display()
            ))
        })?;
        Ok(SigningKey::from_bytes(&seed))
    }

    /// `Ok(None)` when no public key has been generated; `BadKey` when one
    /// exists but is unusable.
    fn load_verifying_key(&self) -> Result<Option<VerifyingKey>, RegistryError> {
        let path = self.pub_path();
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        let raw = hex_decode(text.trim())
            .map_err(|e| RegistryError::BadKey(format!("{}: {e}", path.display())))?;
        let bytes: [u8; PUBLIC_KEY_LENGTH] = raw.try_into().map_err(|_| {
            RegistryError::BadKey(format!(
                "{}: public key must be {PUBLIC_KEY_LENGTH} bytes",
                path.display()
            ))
        })?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|e| RegistryError::BadKey(format!("{}: {e}", path.display())))?;
        Ok(Some(key))
    }
}

/// Manifest-derived fields for a list entry: `(task, scope, eval_runs,
/// problem)`. Any failure lands in `problem`; the other fields stay empty.
fn describe(id: &str, bytes: &[u8]) -> (String, String, usize, Option<String>) {
    let empty = || (String::new(), String::new(), 0);
    let artifact = match Artifact::from_bytes(bytes) {
        Ok(artifact) => artifact,
        Err(e) => {
            let (task, scope, runs) = empty();
            return (task, scope, runs, Some(format!("invalid container: {e}")));
        }
    };
    let actual = artifact.id();
    if actual != id {
        let (task, scope, runs) = empty();
        return (
            task,
            scope,
            runs,
            Some(format!(
                "content id mismatch (tamper evidence): file claims `{id}`, bytes give `{actual}`"
            )),
        );
    }
    match artifact.manifest() {
        Ok(manifest) => (
            manifest.task,
            format!("{}({})", manifest.scope_kind, manifest.scope_name),
            manifest.eval_run_ids.len(),
            None,
        ),
        Err(e) => {
            let (task, scope, runs) = empty();
            (task, scope, runs, Some(format!("unreadable manifest: {e}")))
        }
    }
}

/// Verify the detached signature file over `bytes`. `Err` carries the honest
/// detail: unreadable file, non-hex, wrong length, or failed verification.
fn check_signature(key: &VerifyingKey, bytes: &[u8], sig_path: &Path) -> Result<(), String> {
    let text =
        fs::read_to_string(sig_path).map_err(|e| format!("unreadable signature file: {e}"))?;
    let raw = hex_decode(text.trim()).map_err(|e| format!("signature is not hex: {e}"))?;
    let sig = Signature::from_slice(&raw).map_err(|e| format!("malformed signature: {e}"))?;
    // verify_strict also rejects small-order components; every signature this
    // crate produces passes it.
    key.verify_strict(bytes, &sig)
        .map_err(|e| format!("verification failed: {e}"))
}

/// Write via a sibling tmp file + rename so a crash mid-write cannot leave a
/// truncated `.cbin` posing as an artifact.
fn write_via_tmp(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = dest.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    match fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Content ids are 64 lowercase hex chars (sha-256 of the container bytes).
fn is_content_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

/// Decode hex (either case). `Err` is a plain description.
fn hex_decode(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("odd number of hex digits".into());
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks_exact(2) {
        let hi = hex_val(pair[0]);
        let lo = hex_val(pair[1]);
        match (hi, lo) {
            (Some(hi), Some(lo)) => out.push(hi << 4 | lo),
            _ => return Err("invalid hex digit".into()),
        }
    }
    Ok(out)
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

    fn artifact_bytes(module: &[u8]) -> Vec<u8> {
        let mut entries = BTreeMap::new();
        entries.insert(
            MANIFEST_ENTRY.to_owned(),
            manifest().canonical_json().into_bytes(),
        );
        entries.insert(MODULE_ENTRY.to_owned(), module.to_vec());
        Artifact::new(entries).to_bytes()
    }

    /// Container bytes whose manifest pins `eval_run_ids`; `module` distinguishes
    /// the content id so several such artifacts can coexist in one store.
    fn artifact_bytes_pinning(eval_run_ids: Vec<String>, module: &[u8]) -> Vec<u8> {
        let manifest = Manifest {
            eval_run_ids,
            ..manifest()
        };
        let mut entries = BTreeMap::new();
        entries.insert(
            MANIFEST_ENTRY.to_owned(),
            manifest.canonical_json().into_bytes(),
        );
        entries.insert(MODULE_ENTRY.to_owned(), module.to_vec());
        Artifact::new(entries).to_bytes()
    }

    /// Write `bytes` to a source file and `add` them unsigned; returns the id.
    fn add_bytes(dir: &Path, registry: &Registry, name: &str, bytes: &[u8]) -> String {
        let src = dir.join(name);
        fs::write(&src, bytes).expect("write artifact source");
        registry.add(&src, false).expect("add").id
    }

    /// Tempdir with an open registry and a valid artifact file beside it.
    fn setup() -> (tempfile::TempDir, Registry, PathBuf, Vec<u8>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::open(&dir.path().join("registry")).expect("open");
        let bytes = artifact_bytes(b"not wasm; the registry never executes modules");
        let src = dir.path().join("task.cbin");
        fs::write(&src, &bytes).expect("write artifact file");
        (dir, registry, src, bytes)
    }

    #[test]
    fn keygen_writes_hex_keypair_and_returns_pub_path() {
        let (_dir, registry, _src, _bytes) = setup();
        let pub_path = registry.keygen().expect("keygen");
        assert_eq!(pub_path.file_name().unwrap(), PUB_FILE);
        for name in [KEY_FILE, PUB_FILE] {
            let text = fs::read_to_string(pub_path.parent().unwrap().join(name)).unwrap();
            let trimmed = text.trim();
            assert_eq!(trimmed.len(), 64, "{name} is 32 bytes hex");
            assert!(hex_decode(trimmed).is_ok(), "{name} decodes");
        }
    }

    #[test]
    fn keygen_twice_is_refused() {
        let (_dir, registry, _src, _bytes) = setup();
        registry.keygen().expect("first keygen");
        let err = registry.keygen().unwrap_err();
        assert!(matches!(err, RegistryError::KeyExists { .. }), "{err:?}");
    }

    #[test]
    fn add_unsigned_lists_unsigned_unverified() {
        let (_dir, registry, src, bytes) = setup();
        let outcome = registry.add(&src, false).expect("add");
        assert!(!outcome.signed);
        assert_eq!(fs::read(&outcome.stored_at).unwrap(), bytes);
        let entries = registry.list().expect("list");
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.id, outcome.id);
        assert_eq!(entry.task, "toy-agent");
        assert_eq!(entry.scope, "model_call(fake-frontier)");
        assert_eq!(entry.eval_runs, 1);
        assert!(!entry.signed);
        assert_eq!(entry.verified, None);
        assert_eq!(entry.problem, None);
    }

    #[test]
    fn add_signed_lists_verified_true() {
        let (_dir, registry, src, _bytes) = setup();
        registry.keygen().expect("keygen");
        let outcome = registry.add(&src, true).expect("add --sign");
        assert!(outcome.signed);
        let entries = registry.list().expect("list");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].signed);
        assert_eq!(entries[0].verified, Some(true));
        assert_eq!(entries[0].problem, None);
    }

    #[test]
    fn add_identical_bytes_twice_is_a_noop() {
        let (_dir, registry, src, _bytes) = setup();
        let first = registry.add(&src, false).expect("first add");
        let second = registry.add(&src, false).expect("second add");
        assert_eq!(first.id, second.id);
        assert_eq!(registry.list().expect("list").len(), 1);
    }

    #[test]
    fn get_roundtrips_signed_artifact_byte_identical() {
        let (dir, registry, src, bytes) = setup();
        registry.keygen().expect("keygen");
        let added = registry.add(&src, true).expect("add --sign");
        let out = dir.path().join("out.cbin");
        let outcome = registry.get(&added.id, &out).expect("get");
        assert!(outcome.verified_content);
        assert_eq!(outcome.signature, Some(true));
        assert_eq!(fs::read(&out).unwrap(), bytes);
    }

    #[test]
    fn get_unsigned_artifact_reports_no_signature() {
        let (dir, registry, src, bytes) = setup();
        let added = registry.add(&src, false).expect("add");
        let out = dir.path().join("out.cbin");
        let outcome = registry.get(&added.id, &out).expect("get");
        assert!(outcome.verified_content);
        assert_eq!(outcome.signature, None);
        assert_eq!(fs::read(&out).unwrap(), bytes);
    }

    #[test]
    fn tampered_store_file_is_id_mismatch_and_listed_problem() {
        let (dir, registry, src, _bytes) = setup();
        registry.keygen().expect("keygen");
        let added = registry.add(&src, true).expect("add --sign");
        // Flip the last byte: still a parseable container (the flip lands in
        // module.wasm data), but the content id changes.
        let mut stored = fs::read(&added.stored_at).unwrap();
        *stored.last_mut().unwrap() ^= 0xff;
        fs::write(&added.stored_at, &stored).unwrap();

        let out = dir.path().join("out.cbin");
        match registry.get(&added.id, &out).unwrap_err() {
            RegistryError::IdMismatch { expected, actual } => {
                assert_eq!(expected, added.id);
                assert_ne!(actual, added.id);
            }
            other => panic!("expected IdMismatch, got {other:?}"),
        }
        assert!(!out.exists(), "nothing handed out on failure");

        let entries = registry.list().expect("list");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].problem.is_some());
        // The signature is over the raw bytes, so tampering also fails it.
        assert_eq!(entries[0].verified, Some(false));
    }

    #[test]
    fn corrupt_signature_is_bad_signature() {
        let (dir, registry, src, _bytes) = setup();
        registry.keygen().expect("keygen");
        let added = registry.add(&src, true).expect("add --sign");
        let sig_path = added.stored_at.with_extension("sig");
        let out = dir.path().join("out.cbin");

        // Valid hex, wrong signature.
        fs::write(&sig_path, "0".repeat(128)).unwrap();
        let err = registry.get(&added.id, &out).unwrap_err();
        assert!(matches!(err, RegistryError::BadSignature { .. }), "{err:?}");

        // Not hex at all.
        fs::write(&sig_path, "not hex").unwrap();
        let err = registry.get(&added.id, &out).unwrap_err();
        assert!(matches!(err, RegistryError::BadSignature { .. }), "{err:?}");

        assert!(!out.exists(), "nothing handed out on failure");
        assert_eq!(registry.list().expect("list")[0].verified, Some(false));
    }

    #[test]
    fn get_unknown_or_malformed_id_is_not_found() {
        let (dir, registry, _src, _bytes) = setup();
        let out = dir.path().join("out.cbin");
        for id in ["0".repeat(64), "../escape".into(), "auto".into()] {
            let err = registry.get(&id, &out).unwrap_err();
            assert!(matches!(err, RegistryError::NotFound { .. }), "{err:?}");
        }
    }

    #[test]
    fn add_non_artifact_is_invalid() {
        let (dir, registry, _src, _bytes) = setup();
        let junk = dir.path().join("junk.cbin");
        fs::write(&junk, b"definitely not a container").unwrap();
        let err = registry.add(&junk, false).unwrap_err();
        assert!(matches!(err, RegistryError::InvalidArtifact(_)), "{err:?}");
    }

    #[test]
    fn add_container_with_unreadable_manifest_is_invalid() {
        let (dir, registry, _src, _bytes) = setup();
        let mut entries = BTreeMap::new();
        entries.insert(MANIFEST_ENTRY.to_owned(), b"{}".to_vec());
        entries.insert(MODULE_ENTRY.to_owned(), b"m".to_vec());
        let path = dir.path().join("bad-manifest.cbin");
        fs::write(&path, Artifact::new(entries).to_bytes()).unwrap();
        let err = registry.add(&path, false).unwrap_err();
        assert!(matches!(err, RegistryError::InvalidArtifact(_)), "{err:?}");
    }

    #[test]
    fn sign_without_key_is_no_key_and_stores_nothing() {
        let (_dir, registry, src, _bytes) = setup();
        let err = registry.add(&src, true).unwrap_err();
        assert!(matches!(err, RegistryError::NoKey { .. }), "{err:?}");
        assert!(registry.list().expect("list").is_empty());
    }

    #[test]
    fn add_collision_with_different_stored_bytes_is_id_mismatch() {
        let (_dir, registry, src, _bytes) = setup();
        let added = registry.add(&src, false).expect("add");
        // Simulate tampering: a different (valid) artifact filed under this id.
        let other = artifact_bytes(b"different module bytes");
        let other_id = Artifact::from_bytes(&other).unwrap().id();
        assert_ne!(other_id, added.id);
        fs::write(&added.stored_at, &other).unwrap();
        match registry.add(&src, false).unwrap_err() {
            RegistryError::IdMismatch { expected, actual } => {
                assert_eq!(expected, added.id);
                assert_eq!(actual, other_id);
            }
            other => panic!("expected IdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn one_bad_file_cannot_hide_the_rest() {
        let (_dir, registry, src, _bytes) = setup();
        let added = registry.add(&src, false).expect("add");
        let garbage = added
            .stored_at
            .parent()
            .unwrap()
            .join(format!("{}.cbin", "1".repeat(64)));
        fs::write(&garbage, b"garbage").unwrap();
        let entries = registry.list().expect("list");
        assert_eq!(entries.len(), 2);
        let good = entries.iter().find(|e| e.id == added.id).unwrap();
        assert_eq!(good.problem, None);
        let bad = entries.iter().find(|e| e.id != added.id).unwrap();
        assert!(
            bad.problem
                .as_deref()
                .unwrap()
                .contains("invalid container")
        );
    }

    #[test]
    fn corrupt_public_key_fails_loud() {
        let (dir, registry, src, _bytes) = setup();
        registry.keygen().expect("keygen");
        let added = registry.add(&src, true).expect("add --sign");
        fs::write(registry.pub_path(), "not a key").unwrap();
        let err = registry.list().unwrap_err();
        assert!(matches!(err, RegistryError::BadKey(_)), "{err:?}");
        let err = registry
            .get(&added.id, &dir.path().join("out.cbin"))
            .unwrap_err();
        assert!(matches!(err, RegistryError::BadKey(_)), "{err:?}");
    }

    #[test]
    fn pinned_eval_runs_empty_registry_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::open(&dir.path().join("registry")).expect("open");
        assert!(registry.pinned_eval_runs().expect("pinned").is_empty());
    }

    #[test]
    fn pinned_eval_runs_unions_across_artifacts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::open(&dir.path().join("registry")).expect("open");
        add_bytes(
            dir.path(),
            &registry,
            "a.cbin",
            &artifact_bytes_pinning(vec!["run-a1".into(), "run-shared".into()], b"module A"),
        );
        add_bytes(
            dir.path(),
            &registry,
            "b.cbin",
            &artifact_bytes_pinning(vec!["run-b1".into(), "run-shared".into()], b"module B"),
        );

        let pinned = registry.pinned_eval_runs().expect("pinned");
        let expected: BTreeSet<String> = ["run-a1", "run-b1", "run-shared"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(pinned, expected);
    }

    #[test]
    fn pinned_eval_runs_propagates_id_mismatch() {
        let (_dir, registry, src, _bytes) = setup();
        let added = registry.add(&src, false).expect("add");
        // Tamper the stored bytes: still a parseable container (the flip lands
        // in module data), but the content id no longer matches the filename.
        let mut stored = fs::read(&added.stored_at).unwrap();
        *stored.last_mut().unwrap() ^= 0xff;
        fs::write(&added.stored_at, &stored).unwrap();

        let err = registry.pinned_eval_runs().unwrap_err();
        assert!(matches!(err, RegistryError::IdMismatch { .. }), "{err:?}");
    }

    #[test]
    fn pinned_eval_runs_propagates_invalid_container() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::open(&dir.path().join("registry")).expect("open");
        add_bytes(
            dir.path(),
            &registry,
            "a.cbin",
            &artifact_bytes_pinning(vec!["run-a1".into()], b"module A"),
        );
        // A garbage file filed under a well-formed id: `list` reports it as a
        // per-entry problem, but the protection set must refuse loudly rather
        // than return an incomplete set.
        let garbage = registry.artifact_path(&"1".repeat(64));
        fs::write(&garbage, b"not a container").unwrap();

        let err = registry.pinned_eval_runs().unwrap_err();
        assert!(matches!(err, RegistryError::InvalidArtifact(_)), "{err:?}");
    }
}
