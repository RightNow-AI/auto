# ADR-0008: registry — local content-addressed directory, detached ed25519 signatures

status: accepted · scope: `crates/auto-registry`,
`crates/auto-cli` (`registry`), `spec/registry.md`, `spec/manifest.md`

## context

S7 gives artifacts a home and a trust story: the registry, signing, and the
manifest standard. Content addressing already exists — the artifact id is
the sha-256 of the container bytes (ADR-0004) — so the registry's job is to
store by that identity, prove tamper on the way out, and bind a signer to
the bytes. Requirements: tamper evidence must be structural (recomputed from
bytes, not trusted metadata); adding trust metadata must not change an
artifact's identity — the same artifact must never have two ids; no network
anywhere (the constitution's no-network test rule, and the local-first loop
is not proven yet); and the key story must be honest — no sigstore
infrastructure exists locally, and pretending otherwise would be a mock
wearing a trust layer's name.

## decision

Five coupled choices:

1. **A local directory, plain files:** `artifacts/<id>.cbin`,
   `artifacts/<id>.sig`, `keys/`. The registry's state is the filesystem;
   nothing else is authoritative.
2. **Detached ed25519 signatures** (`ed25519-dalek` 2.2, RFC 8032) over the
   container bytes exactly as stored. Detached is load-bearing: **signing
   never changes the artifact id.**
3. **`get` recomputes the digest** of the bytes it serves and refuses on
   mismatch — corruption is an id-mismatch error, never a warning and never
   silently served.
4. **One local keypair** created by `registry keygen`; no rotation, no
   revocation, no multi-key trust. A signature proves "this registry's key
   signed these bytes", nothing more, and the spec says so.
5. **Listings recomputed from files on every read**, problems surfaced per
   entry (unreadable container, missing/failed signature) rather than
   entries silently skipped.

## alternatives considered

**sigstore keyless signing.** The constitution names sigstore, and it
remains the recorded target. Deferred: keyless signing is an OIDC identity
flow against Fulcio (short-lived certificates) plus a Rekor transparency-log
write — network services and an account identity in the signing path, both
against the local-first, no-network posture while the loop is proven on one
machine. The detached-signature layout is chosen to map onto sigstore
bundles later: the artifact bytes and their id are untouched by whatever
envelope carries the trust material.

**Embedded signatures (inside the container).** Self-contained artifacts,
no sidecar files. Rejected outright: writing a signature into the container
changes the container bytes — i.e. the content id — so signed and unsigned
copies of the same artifact would be *different artifacts*. That forks the
one-artifact-one-id rule content addressing exists to keep, and re-signing
would re-identify. The signature must wrap the id, never sit inside it.

**A sqlite index (like the trace store).** Faster listings, queryable
metadata, one familiar dependency. Rejected as YAGNI: every fact a listing
needs is recomputable from the stored files at local volumes, and an index
is a second source of truth that can drift from the bytes — the exact
failure mode content addressing removes. The trace store earned sqlite with
real query patterns (ADR-0002); the registry has not. Revisit when a
registry holds enough artifacts that recomputation measurably hurts.

**minisign/signify as an external signer.** Battle-tested detached-signature
tools. Rejected: shelling out to a platform-dependent binary for what one
audited, workspace-pinned pure-rust crate does in-process; and the trainer
subprocess precedent (ADR-0006) exists to swap *heavy, replaceable*
machinery, which a 64-byte signature is not.

## consequences

- Trust is honestly thin: possession of one key file. Key compromise means
  re-key and re-sign; there is no revocation story until rotation lands
  (open questions).
- The registry is per-machine. Distribution, transport, and foreign-key
  trust policy are all future work, recorded, not implied.
- The signature covers bytes, not meaning: manifest honesty is still
  carried by the manifest standard (spec/manifest.md §4) — a consumer
  verifies the digest, the manifest, and the linkage regardless of any
  signature.
- Tamper detection on `get` is unconditional (digest); signature
  verification adds signer evidence where a `.sig` exists. The e2e proves
  the tamper negative by flipping one stored byte.
- No index means listing cost grows linearly with stored artifacts;
  accepted at local volumes, measured before it is ever "fixed".

## sources

- `ed25519-dalek` 2.2 (SigningKey / VerifyingKey, detached `Signature`,
  `sign`/`verify`): <https://docs.rs/ed25519-dalek/2.2.0/ed25519_dalek/>
- RFC 8032 — Edwards-Curve Digital Signature Algorithm (Ed25519):
  <https://www.rfc-editor.org/rfc/rfc8032>
- sigstore — keyless signing, Fulcio CA, Rekor transparency log:
  <https://www.sigstore.dev/> and <https://docs.sigstore.dev/>
