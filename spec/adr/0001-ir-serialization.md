# ADR-0001: IR serialization — flatbuffers

status: accepted · scope: `crates/auto-ir`, `spec/ir.md`

## context

The IR is the interchange layer of the whole toolchain (CLAUDE.md: "whoever
owns the IR owns the layer"). Its wire format must provide:

1. **Byte-stable round-trip** — a hard invariant (goldens, content addressing
   in the registry later, diffability of compiled artifacts).
2. **Safe reads of untrusted input** — `.air` files and, later, registry
   artifacts arrive from outside; decode must verify before touching bytes.
3. **Cross-language reach** — python/ts trace SDKs and future tooling must be
   able to read the same buffers from one schema.
4. **Schema evolution** — the format will grow through S1–S7 without
   invalidating archived traces and artifacts.

## decision

Flatbuffers, using the official toolchain: `flatc` **25.12.19** generating
rust in `build.rs`, with the `flatbuffers` runtime crate pinned **exactly**
(`=25.12.19`) — generated code and runtime are released in lockstep, and the
golden bytes depend on both. `build.rs` refuses to build with any other flatc
version (resolution: `FLATC` env → `tools/flatc/` → PATH).

Canonicality is **our** responsibility on top of the format: flatbuffers does
not define a canonical encoding, but its builder is deterministic given a
deterministic write sequence. `auto-ir` imposes the sequence (ordered
containers, fixed field order — spec/ir.md §9), which yields equal-graphs ⇒
equal-bytes and byte-stable round-trips. This property is pinned by golden
files and a proptest round-trip property, so a regression in either our
writer or the upstream builder fails CI immediately.

## alternatives considered

**serde + bincode.** Least friction in pure rust (no codegen, no external
binary). Rejected: rust-only (no schema for the python/ts SDKs to share); no
verifier for untrusted input (deserialization of hostile bytes is a
denial-of-service surface at minimum); byte layout is an artifact of struct
definition order and serde impl details, so "byte-stable" would be one
refactor away from silently breaking; no schema-evolution story beyond
hand-rolled version enums.

**protobuf (prost).** Best ecosystem and evolution rules. Rejected on the
hard invariant: protobuf explicitly does **not** guarantee canonical /
deterministic serialization across implementations or library versions
(deterministic modes exist per-implementation and are documented as unstable
across versions); unknown-field retention and map ordering make byte identity
a non-goal of the format. Content addressing on top of protobuf means
canonicalizing ourselves anyway — at which point the format buys less than
its weight.

**cap'n proto.** Zero-copy like flatbuffers, strong RPC story. Rejected:
canonical form exists but interacts with segment allocation and packing, so
byte identity again requires our own canonicalization discipline plus a less
mainstream toolchain (`capnpc` binary + smaller rust/python ecosystem); the
capability/RPC features it shines at are not what the IR needs. Net: same
codegen-binary cost as flatbuffers, weaker verifier story in rust, less reach.

**flatbuffers (chosen).** Zero-copy reads with a **buffer verifier** in the
rust runtime (untrusted input is verified before any access); multi-language
codegen from one `.fbs`; append-only schema evolution; deterministic builder
we can drive canonically; file identifiers (`AIR0`) for cheap sniffing. Cost:
`flatc` is a build-time binary dependency and pure-`cargo build` is lost.
Mitigations: pinned version enforced by `build.rs` with loud failure and
install pointers; gitignored `tools/flatc/` for local installs; CI installs
the pinned official release binary. (A pure-rust flatbuffers implementation,
`planus`, could remove the binary dependency later without changing the wire
format; not adopted now — smaller ecosystem, and S0 optimizes for the
reference toolchain.)

## consequences

- flatc 25.12.19 and `flatbuffers = "=25.12.19"` move together, always; any
  bump re-blesses goldens deliberately and gets an ADR entry + schema-version
  decision per spec/ir.md §10.
- Byte canonicality is enforced by our writer + tests, not assumed from the
  format. The invariant lives in `roundtrip_prop.rs` and `golden.rs`.
- Contributors need one extra binary; the build tells them exactly which and
  from where.

## sources

- flatbuffers release v25.12.19 (2025-12-19), assets incl.
  `Windows.flatc.binary.zip`, `Linux.flatc.binary.clang++-18.zip`:
  <https://github.com/google/flatbuffers/releases/tag/v25.12.19>
- `flatbuffers` crate 25.12.19 (2025-12-19, "Official FlatBuffers Rust
  runtime library"): <https://crates.io/crates/flatbuffers>
- verifier + rust usage: <https://flatbuffers.dev/> (rust section); verifier
  defaults confirmed in crate source `flatbuffers-25.12.19/src/verifier.rs`
  (max_depth 64, max_tables 1_000_000).
- `proptest` 1.11.0 (2026-03-24): <https://crates.io/crates/proptest>
- `clap` 4.6.1 (2026-04-15): <https://crates.io/crates/clap>
- `thiserror` 2.0.18 (2026-01-18): <https://crates.io/crates/thiserror>
- protobuf on (non-)canonical serialization:
  <https://protobuf.dev/programming-guides/serialization-not-canonical/>
