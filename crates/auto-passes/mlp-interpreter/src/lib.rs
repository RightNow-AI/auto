//! MLP interpreter — auto-model's neural inference compiled for artifacts.
//! Same frozen ABI + `init` extension as the other interpreters: `init`
//! loads the artifact's init payload (here: mlp json), `run` infers on
//! canonical-JSON input bytes. Every failure traps — an honest execution
//! failure. Zero imports by construction.

use std::sync::OnceLock;

use auto_model::Mlp;

static MLP: OnceLock<Mlp> = OnceLock::new();

/// Bump allocator for host → module byte transfer. Leaked on purpose:
/// instances are single-shot.
// unsafe(no_mangle) justification: frozen ABI export name; single definition
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: u32) -> u32 {
    let buffer = vec![0u8; len as usize].into_boxed_slice();
    // leak: region must stay valid for the host; instance is discarded after
    Box::leak(buffer).as_ptr() as u32
}

/// Load the mlp (canonical mlp json). Once per instance, before `run`.
// unsafe(no_mangle) justification: frozen ABI export name (init extension)
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn init(ptr: u32, len: u32) {
    let bytes = read_region(ptr, len);
    let text = std::str::from_utf8(bytes).expect("mlp bytes must be utf-8");
    let mlp = Mlp::from_json(text).expect("mlp json must parse");
    MLP.set(mlp)
        .expect("init must be called exactly once per instance");
}

/// Infer on canonical-JSON input bytes; returns `((ptr as u64) << 32) | len`.
// unsafe(no_mangle) justification: frozen ABI export name
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn run(ptr: u32, len: u32) -> u64 {
    let mlp = MLP.get().expect("run requires init to have loaded an mlp");
    let bytes = read_region(ptr, len);
    let text = std::str::from_utf8(bytes).expect("input bytes must be utf-8");
    let input: serde_json::Value = serde_json::from_str(text).expect("input must be JSON");
    // pure matmul + relu + argmax over counted trigram features
    let output = auto_model::infer_mlp(mlp, &input).expect("inference failed");
    let out = serde_json::to_string(&output)
        .expect("output serialization cannot fail")
        .into_bytes()
        .into_boxed_slice();
    let out_len = u32::try_from(out.len()).expect("output fits u32");
    // leak: region must outlive the call for the host to read
    let out_ptr = Box::leak(out).as_ptr() as u32;
    (u64::from(out_ptr) << 32) | u64::from(out_len)
}

/// View a host-written region inside this module's linear memory.
#[allow(unsafe_code)] // raw-pointer view of caller-provided (ptr, len); wasm32 linear memory
fn read_region(ptr: u32, len: u32) -> &'static [u8] {
    // SAFETY: (ptr, len) designate bytes inside this module's own linear
    // memory, written by the host immediately before the call; memory never
    // shrinks and the instance is single-shot.
    unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) }
}
