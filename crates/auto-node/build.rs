fn main() {
    // napi_build::setup() emits the platform-specific link configuration a
    // Node addon needs (on Windows: the N-API symbols resolve against the
    // host node.exe at dlopen, not against an import library at link time).
    // Gated on the `node` crate feature — the plain cargo gates
    // (check/clippy/test, feature OFF) build no FFI and must need no Node
    // toolchain (spec/adr/0026-napi-embedding.md). Build scripts see crate
    // features only as CARGO_FEATURE_* env vars, hence the env test.
    if std::env::var_os("CARGO_FEATURE_NODE").is_some() {
        napi_build::setup();
    }
}
