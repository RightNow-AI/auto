# ADR-0004: artifact execution — custom container, pure core wasm, wasmtime

status: accepted · scope: `crates/auto-backend`, `crates/auto-runtime`, `spec/artifact.md`, `evals/toy-agent/fake-frontier-impl`

## context

S3 emits the first `.cbin` and executes it: the container format, the module
ABI, the execution engine, its resource bounds, and the gate that decides
whether an artifact may exist at all. Requirements: the artifact id is a
content address, so the byte format must be deterministic end to end;
capability confinement must be physical, not advisory (CLAUDE.md: "a binary
physically cannot exceed its declared capabilities"); execution during
verification must be bounded (a runaway candidate must fail, not hang CI);
no network inside any of it; and the S3 spine item explicitly allows
hand-assisted passes while forbidding mocks that pretend to be the compiler.

## decision

Five coupled choices:

1. **A custom minimal container** (`ACB0` magic, LE lengths, sorted unique
   named entries — spec/artifact.md §2) rather than zip or tar. Artifact
   id = sha-256 of the container bytes.
2. **Core wasm module with a zero-imports purity rule now; component model
   later.** The frozen v0 ABI is `memory`/`alloc`/`run` with canonical-JSON
   byte payloads and a packed `(ptr << 32) | len` return; any import refuses
   at load. The constitution names the wasm component model as the backend
   target — the migration is deliberate and recorded, not forgotten.
3. **wasmtime 46.0.1 as the embedded engine**, the same engine the
   constitution already mandates for synthesis sandboxes.
4. **Fuel + store memory limits** (500,000,000 fuel per call, 64 MiB cap)
   rather than epoch-based deadlines.
5. **A hand-assisted compile gate at S3:** the emit gate, differential
   checks, and manifest honesty rules are real, mechanical code; the wasm
   module they gate is hand-written (`fake-frontier-impl`). Synthesis lands
   S4 and slots in behind the same gate.

## alternatives considered

**zip as the container.** Universal tooling. Rejected: zip carries per-entry
timestamps, permission bits, compression choices, and free entry ordering —
four sources of byte nondeterminism that fight content addressing; every
writer must be forced into a canonical corner, and third-party repackers
silently fall out of it. The same artifact must never have two ids.

**tar (+ gzip).** Same shape of objection: ustar/pax headers carry mtime,
uid/gid, and mode; pax vs gnu extension variance adds more. "Deterministic
tar" is a discipline imposed on a format that resists it, and gzip adds its
own header timestamp. A 20-line format with sorted unique entries and two
integer widths is deterministic by construction and auditable in one
sitting — the same reasoning that made `auto-ir` impose canonical encoding
on top of flatbuffers (spec/ir.md §9) rather than trust a format's defaults.

**wasm component model now.** The declared destination: typed interfaces
(wit), canonical ABI, no hand-packed pointers. Rejected *for v0 timing
only*: the component toolchain (cargo-component, wit-bindgen, canonical ABI
lifting/lowering) is heavy machinery for a spine item that needs exactly one
pure `json → json` function, and hand-freezing a two-export core ABI is a
smaller total surface than adopting the component stack early. Consequence
accepted: a real migration later (new module shape, manifest bump, ADR).

**wasmer / non-rust embeddings (wazero-style).** wasmer is a capable rust
alternative; wazero's zero-dependency embedding story is go-native and would
put a foreign runtime inside a rust core. Rejected: wasmtime is the
Bytecode Alliance reference embedding, actively maintained, with first-class
fuel metering and store limits — and the constitution already pins wasmtime
for synthesis sandboxes, so a second engine would mean two behaviors to
trust instead of one.

**wasmi (interpreter).** Much lighter to carry (no cranelift). Rejected:
interpretation is markedly slower, and the measured compiled-latency numbers
in manifests are part of the product's claim; we already carry cranelift via
the wasmtime dependency either way. Revisit only if embedding weight ever
matters more than execution speed.

**epoch deadlines instead of fuel.** wasmtime's epoch interruption is the
lower-overhead cancellation tool, but it needs a ticker thread advancing the
epoch and binds bounds to wall-clock scheduling. Fuel counts instructions:
no extra thread, and a deterministic-ish bound tied to executed work, which
is the right shape for verification (the same candidate exhausts fuel the
same way on every machine). The fuel-counting overhead is acceptable at
verification volumes. Memory is capped separately via store limits either
way.

## consequences

- The container format is ours to maintain — acceptable at ~150 lines
  including strict parsing, and pinned by round-trip tests.
- Purity is total in v0: nothing impure can be expressed, so capability
  plumbing (WASI grants mapped from IR effects) is deferred whole, tracked
  in open-questions.
- Fuel units are wasmtime-internal and may shift meaning across engine
  upgrades; the constant bounds runaways, it is not a portable performance
  contract. Engine bumps re-check the fuel headroom of known artifacts.
- The component-model migration is a known format break: new module shape,
  `manifest_version` bump, ADR. Deliberate debt, on the books.
- The gate machinery (emit refusal on Fail/Inconclusive, differential
  replay, measured-numbers-only manifests) is what S4 synthesis will be
  verified by — S3 proves the gate on a hand-supplied module, which is
  exactly what "hand-assisted" is allowed to mean and no more.

## sources

- `wasmtime` crate 46.0.1: <https://crates.io/crates/wasmtime>
- fuel metering (`Config::consume_fuel`, `Store::set_fuel`, exhaustion
  traps): <https://docs.rs/wasmtime/46.0.1/wasmtime/struct.Store.html#method.set_fuel>
- store limits (`StoreLimits`, `StoreLimitsBuilder::memory_size`,
  `Store::limiter`): <https://docs.rs/wasmtime/46.0.1/wasmtime/struct.StoreLimitsBuilder.html>
- component model docs (wit, canonical ABI, tooling):
  <https://component-model.bytecodealliance.org/>
- container determinism precedent: spec/ir.md §9 (canonical encoding imposed
  on top of the serialization format; ADR-0001).
