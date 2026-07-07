# Auto registry — specification, v0

Status: v0, matches `crates/auto-registry` and the `auto registry` surface
of `crates/auto-cli` as merged. Where prose and code disagree, the code
wins; this document is written for external readers.

The **registry** is where artifacts live between compile and run. The
constitution's registry is "content-addressed, sigstore-signed"; v0 ships
the smallest honest member of that family: a **local directory** of
content-addressed artifacts with **detached ed25519 signatures** from a
single local keypair. Content addressing is real, tamper evidence is real,
signing is real; sigstore, remoteness, and trust policy are recorded
targets, not claims (§5).

## 1. layout

```text
<root>/
  artifacts/<id>.cbin   # the container bytes, named by their sha-256
  artifacts/<id>.sig    # detached ed25519 signature over those bytes (per-artifact, optional)
  keys/                 # the local signing keypair (`auto registry keygen`)
```

Plain files, no database: the registry's state **is** the filesystem.
Listings and verification are recomputed from the bytes on every read —
there is no index to drift from the truth (ADR-0008). Exact key-file names
inside `keys/` are the implementation's; code wins.

## 2. content addressing

An artifact's id is the **sha-256 (lowercase hex) of its container bytes**
— the same id defined in spec/artifact.md §2; the registry invents no
second identity. Two consequences, both load-bearing:

- **`add` is idempotent by construction.** The same bytes store under the
  same id; adding an artifact twice changes nothing.
- **`get` recomputes the digest of the bytes it is about to serve** and
  refuses on mismatch. A flipped bit in a stored artifact is an **id
  mismatch error**, never silently served — tamper evidence is structural,
  not a policy check. (The toy e2e proves the negative: corrupt one byte,
  `get` fails.)

## 3. signatures

Signatures are **detached**: `artifacts/<id>.sig` holds an ed25519
signature (RFC 8032, via `ed25519-dalek`) over the container bytes exactly
as stored. Detachment is the point — **signing never changes the artifact
id**, because the signature lives outside the container. Signed and
unsigned copies of the same artifact are the same artifact; an embedded
signature would fork the identity content addressing exists to keep single
(ADR-0008).

**Key management v0, honestly:** one local keypair under `keys/`, created
by `auto registry keygen`. No rotation, no revocation, no multi-key trust,
no identity binding beyond possession of the key file. A verified signature
here proves exactly "signed by this registry's key" — nothing about *who*
that is. The recorded destination is sigstore keyless signing (OIDC-bound
identity, transparency log), which needs network and identity
infrastructure this local-first toolchain deliberately does not touch yet.

A signature complements the §2 digest check, never replaces it: the digest
says the bytes are the ones addressed; the signature says this registry's
key vouched for them.

## 4. entries and problem surfacing

`list` renders one entry per stored artifact: the id, what it implements
(task and scope, read from the manifest), and its signature status
(verified / unsigned / failed). Problems are **surfaced, never skipped**:
an unreadable container, an unparseable manifest, a missing or failing
signature appear as that entry's reported problem instead of the entry
silently vanishing — a registry that hides its broken entries is lying
about its inventory. Exact rendering strings: code wins.

`get` serves the stored bytes to `--out`, gated on the §2 digest check.
Signature verification is reported where a signature and key exist; the
digest check is unconditional.

## 5. what v0 is not

Recorded in spec/adr/open-questions.md ("registry (S7)"):

- **Remote transport is loopback-only (§6, ADR-0022).** A served registry
  root now speaks a frozen HTTP push/pull protocol, content verified at both
  ends — but with **no authentication and no TLS**: a development transport
  for a loopback or trusted-LAN registry, not an open one. `cargo test`
  binds only loopback listeners; nothing else in the toolchain reaches the
  network.
- **Not sigstore.** Keyless signing and transparency logs are the named
  target; the detached-signature layout is chosen to map onto sigstore
  bundles when that lands.
- **No key rotation or trust policy.** One key, no revocation, no notion of
  which foreign keys to trust.
- **No GC or retention.** Nothing is ever deleted — including the eval-run
  records that manifests pin (spec/contract.md §7), which must outlive
  every artifact citing them.

Rationale and alternatives: spec/adr/0008-registry-signing.md.

## 6. remote transport v0 (ADR-0022)

A served registry root speaks HTTP so a second machine — or a fresh local
root — can **push** artifacts to it and **pull** artifacts from it. The
registry stays content-addressed on the wire: an id is still the sha-256 of
the container bytes (§2), so integrity is **structural** and survives the
transport unchanged. Blocking `tiny_http` server, blocking `ureq` client,
no tokio — the same synchronous stack as `auto serve` / `auto proxy`.

**Frozen protocol** (`/v0`; the CLI verbs wire against exactly this):

| method | path | success | else |
| --- | --- | --- | --- |
| GET | `/v0/artifacts` | 200 `text/plain`, one 64-hex id per line, sorted | — |
| GET | `/v0/artifacts/<id>` | 200 octet-stream, verified bytes | 404 absent |
| GET | `/v0/artifacts/<id>/signature` | 200 octet-stream, detached sig | 404 unsigned |
| GET | `/v0/key` | 200 octet-stream, the verifying key (as stored in `auto.pub`) | 404 no key |
| PUT | `/v0/artifacts/<id>` | 201 stored · 200 idempotent | 400 digest mismatch · 409 id present with different bytes |
| PUT | `/v0/artifacts/<id>/signature` | 201 accepted | 400 (no artifact, no key, or bad signature) |

A non-64-hex id is `400` (it can never name a stored artifact and must not
traverse paths); any other route or method is `404`. There is **no auth and
no TLS** — stated loudly here and in ADR-0022: this is a development
transport for a loopback or trusted-LAN registry. Production
authentication, transport encryption, and sigstore keyless signing are
recorded targets; the detached-signature layout (§3) is the transport this
last will ride.

**Verified at both ends.** Integrity is checked by whoever holds the bytes,
so tampering is caught wherever it happens:

- **Serving (GET artifact)** goes through the ordinary `get` path (§2): the
  server re-parses, recomputes the content id, and verifies any signature
  before a byte leaves. A corrupt or tampered-at-rest entry is a `500` with
  the error, **never silent bytes**.
- **Uploading (PUT artifact)** recomputes the content id from the body and
  refuses (`400`, writing nothing) if it differs from the path id. Identical
  bytes already present are an idempotent `200`; a first store is `201`; an
  id already present with *different* bytes is `409` — tamper evidence on the
  server's own store.
- **Pushing** fetches the local copy through `get` first, so a locally
  corrupt artifact is refused before it reaches the wire. A signed artifact's
  detached signature is pushed too, and the server stores it only if it
  verifies against the server's key.
- **Pulling** recomputes the id from the received bytes (mismatch → refuse,
  write nothing), fetches the signature and the verifying key, and verifies
  the signature **before** writing anything. The artifact is then stored
  through the ordinary `add` path (all local invariants hold) and the whole
  thing re-verified through `get`.

**Signatures across a boundary.** A `.sig` verifies against *a* verifying
key; the registry model is one key per root (§3). So a pushed signature is
accepted only when it verifies against the server's key (the pushing client
and the server share a trust root — in a real deployment, the org key), and
a pull installs the remote's verifying key as the destination's `auto.pub`
when that root has none, refusing loudly on a **conflict** with a different
local key rather than overwriting a trust root. Content addressing needs no
such sharing — a pulled artifact's bytes are verified by recomputing their
id regardless of any key.

Proof: `crates/auto-registry` unit tests exercise every route on the pure
handler (socket-free); one loopback integration test binds a real
`127.0.0.1:0` server, pushes, pulls into a second root, and shows a tampered
server file makes the pull refuse. `evals/registry-remote/e2e.sh` proves the
same over the CLI and real sockets. Rationale and alternatives:
spec/adr/0022-remote-registry.md.
