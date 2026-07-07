# ADR-0022: remote registry — loopback HTTP transport, content verified at both ends

status: accepted · scope: `crates/auto-registry`
(`remote`), `crates/auto-cli` (`registry serve|push|pull`),
`spec/registry.md`, `evals/registry-remote`

## context

The registry (ADR-0008) is a local content-addressed directory: artifacts
named by the sha-256 of their container bytes, detached ed25519 signatures
beside them, one keypair per root. "Remote registries" have been the named
S7 target since it landed — an artifact compiled on one machine has to reach
another. This ADR ships the transport, and only the transport: move bytes
and signatures between registry roots over HTTP without weakening the one
property the whole store rests on — that an id is the digest of the bytes,
so tampering is *structural*, not a metadata check.

Requirements: integrity must survive the wire (both ends recompute the id
and refuse a mismatch — the transport is untrusted); the on-disk store's
invariants must hold for anything that lands in it (writes go through the
existing `add`/`get` paths, not a side door); the protocol must be frozen so
the CLI and any other client wire against a stable contract; and the honesty
bounds must be loud — there is no authentication and no TLS yet, so this is a
development transport for a loopback or trusted-LAN registry, not an open
one. Sigstore keyless signing (ADR-0008) stays the recorded destination;
this is the transport it will ride.

## decision

A **blocking HTTP transport** in `auto-registry::remote`, same synchronous
stack as `auto serve` / `auto proxy` (`tiny_http` server, `ureq` client, no
tokio; ADR-0010/0011). Five coupled choices:

1. **A frozen `/v0` protocol.** The CLI verbs wire against exactly this:

   | method | path | success | else |
   | --- | --- | --- | --- |
   | GET | `/v0/artifacts` | 200 text, one 64-hex id/line, sorted | — |
   | GET | `/v0/artifacts/<id>` | 200 octet-stream, verified bytes | 404 |
   | GET | `/v0/artifacts/<id>/signature` | 200 octet-stream sig | 404 |
   | GET | `/v0/key` | 200 octet-stream verifying key | 404 |
   | PUT | `/v0/artifacts/<id>` | 201 stored · 200 idempotent | 400 digest · 409 conflict |
   | PUT | `/v0/artifacts/<id>/signature` | 201 accepted | 400 |

   A non-64-hex id is `400` (never names an artifact; must not traverse
   paths); every other route or method is `404`.

2. **A pure request handler.** `RegistryHost::handle(method, path, body) ->
   Reply` touches no socket, so every route is unit-tested without binding a
   port; `serve` is the thin `tiny_http` shell (blocking, sequential accept
   loop, one log line per request), exactly the `auto serve` split.

3. **Verified at both ends — reuse `get`/`add`, never a side door.**
   Serving an artifact goes through `Registry::get` (re-parse, recompute id,
   verify signature): a corrupt or tampered-at-rest entry is a `500`, never
   silent bytes. A `PUT` recomputes the content id from the body (the strict
   container parse equals sha-256 of the body for any artifact the store
   accepts) and refuses `400` on mismatch, writing nothing; storage goes
   through `Registry::add`, so idempotence (`200`), first-store (`201`), and
   the different-bytes-under-one-id case (`409`, tamper evidence) fall out of
   the store's own logic.

4. **The client verifies too.** `push` fetches the local copy through `get`
   first (a locally corrupt artifact never reaches the wire). `pull`
   recomputes the id from the received bytes (mismatch → refuse, nothing
   written), fetches and verifies the signature against `GET /v0/key`
   *before* writing, stores through `add`, then re-verifies the written copy
   through `get`.

5. **One key per root, reconciled on pull.** A `.sig` verifies against one
   verifying key (ADR-0008). A pushed signature is accepted only if it
   verifies against the server's key (client and server share a trust root —
   in a deployment, the org key). A pull installs the remote's verifying key
   as the destination's `auto.pub` when that root has none, and refuses
   loudly on a conflict with a *different* local key rather than overwrite a
   trust root. Content addressing needs no shared key — bytes are verified by
   recomputing their id regardless.

## alternatives considered

**An OCI registry (the OCI distribution spec).** Artifacts as OCI blobs,
digest-addressed by `sha256:…`, pushed/pulled by any container tooling — a
mature, ubiquitous distribution API that already speaks content addressing.
The recorded destination for a *public* registry, and the natural home once
sigstore lands (cosign signs OCI artifacts). Deferred here: the full
distribution spec (chunked/monolithic blob uploads, manifests, tag lists,
content negotiation, auth via bearer-token flows) is a large surface to
implement or a large dependency to pull in, for a v0 whose job is to move
bytes between two `auto` roots on a trusted network. Our id is already a
sha-256; mapping it onto `sha256:` descriptors later is mechanical.

**rsync / plain file sync.** `rsync` between `artifacts/` directories is
close to free and preserves bytes exactly. Rejected as the *protocol*: it
transports a directory, not a registry — nothing recomputes ids, verifies
signatures, or enforces the store invariants at the boundary, so a corrupt
or mis-filed file syncs as readily as a good one. The value here is the
verified push/pull semantics, not the byte movement; rsync is a fine
*operational* copy of a whole root, and nothing forbids it.

**git-lfs.** Version large artifacts as LFS pointers in a git repo. Rejected:
it couples the registry to a git remote and its auth, versions by commit
rather than by content id (a second identity beside the sha-256), and drags
in a large client — all to reach a workflow (browse, pull one artifact by
id) the frozen HTTP protocol serves directly.

**Embedding the transport in `auto serve`.** `auto serve` already binds
`tiny_http` over a registry (ADR-0011), so bolt push/pull onto it. Rejected:
`auto serve` is the tier-1 *execution* face (guard-gated `/run`); a registry
*transport* is a different concern (move artifacts, do not run them) with a
different verb (`registry`), a different protocol, and no executor. Sharing
the socket would entangle two independent surfaces.

## non-goals (recorded, not shipped)

- **Authentication.** No tokens, no mTLS, no per-client identity. Anyone who
  can reach the socket can read and write. Loopback / trusted-LAN only.
- **TLS.** Plaintext HTTP; bytes are integrity-checked by digest, not
  encrypted or authenticated in transit. A network attacker cannot slip
  corrupt bytes past the id check, but can read traffic and (absent auth)
  write artifacts.
- **Sigstore keyless signing.** Still the recorded target (ADR-0008); this
  transport is what carries the detached signatures a sigstore bundle will
  later replace.
- **Deletion / GC over the wire, foreign-key trust policy, concurrent
  writers.** The accept loop is sequential (v0, like `auto serve`); a
  multi-writer server and a real trust model are future work.

## consequences

- Integrity is transport-independent: the id check fires at whichever end
  holds the bytes, so a tampered artifact — in flight or at rest on the
  server — is refused, proven by the loopback test (tamper the server file →
  pull refuses) and the e2e.
- Signatures cross a boundary only under a shared trust root; the pull-time
  key reconciliation makes that explicit (install-if-absent, refuse on
  conflict) instead of silently trusting or silently dropping.
- The server holds no execution surface and no key material it must protect
  beyond `auto.pub` (it verifies pushed signatures, it never signs) — a
  smaller blast radius than `auto serve`.
- Because there is no auth, deploying this beyond loopback is an operator
  decision the spec refuses to paper over; the log line and startup banner
  say "loopback dev transport, NO auth, NO TLS".

## sources

- `tiny_http` 0.12 (blocking HTTP server; `Server::http`, `Request`,
  `Response`) — workspace pin, same as `auto-serve`/`auto-proxy`:
  <https://docs.rs/tiny_http/0.12.0/tiny_http/>
- `ureq` 3.3 (blocking HTTP client; `Agent`, `get`/`put`, `Body`) — workspace
  pin, same as the frontier client: <https://docs.rs/ureq/3.3.0/ureq/>
- OCI distribution specification (digest-addressed blob push/pull, the
  recorded public-registry target):
  <https://github.com/opencontainers/distribution-spec>
- git-lfs (large-file versioning over git):
  <https://git-lfs.com/>
- rsync (directory sync): <https://rsync.samba.org/>
