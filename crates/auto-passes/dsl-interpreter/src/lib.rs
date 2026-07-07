//! Generic DSL interpreter — the wasm side of "one implementation, two
//! compilations" (auto-dsl's evaluator compiled for artifacts).
//!
//! Implements the frozen artifact ABI (spec/artifact.md §ABI) plus the
//! additive `init` extension: the host calls `init(ptr, len)` once per
//! instance with the artifact's `program.json` bytes before calling `run`.
//! Every failure (no program loaded, bad program json, bad input json, eval
//! error) is a panic, which traps — an honest execution failure the host
//! reports; nothing is swallowed.
//!
//! Zero imports by construction: this crate links no host functions, so the
//! module physically cannot perform I/O.

use std::sync::OnceLock;

use auto_dsl::Payload;

// The ONE capability import (ADR-0017), present ONLY in the `tools` build:
// the host executes a declared tool and returns a packed (ptr << 32 | len)
// region holding an envelope {"ok":<value>} | {"err":"..."} written into
// this module's memory via its own alloc. The default build declares no
// imports at all — pure artifacts stay physically pure.
// unsafe justification: a wasm import declaration is inherently an unsafe
// extern boundary; the frozen capability ABI (ADR-0017) defines its contract
#[allow(unsafe_code)]
#[cfg(feature = "tools")]
#[link(wasm_import_module = "auto")]
unsafe extern "C" {
    fn tool_call(ptr: u32, len: u32) -> u64;
}

/// Tool seam for [`auto_dsl::eval_payload`]: marshal `{"name","input"}` to
/// the host import and unwrap the envelope. Only tool stages reach this.
// unsafe justification: calling the declared import; (ptr, len) designate a
// leaked region of this module's own linear memory per the frozen ABI
#[allow(unsafe_code)]
#[cfg(feature = "tools")]
fn host_tool(name: &str, input: &serde_json::Value) -> Result<serde_json::Value, String> {
    let request = serde_json::json!({ "name": name, "input": input }).to_string();
    let bytes = request.into_bytes().into_boxed_slice();
    let len = u32::try_from(bytes.len()).expect("request fits u32");
    let ptr = Box::leak(bytes).as_ptr() as u32;
    // SAFETY: the frozen capability ABI — the host reads (ptr, len) from this
    // module's linear memory and returns a packed region it wrote via alloc
    let packed = unsafe { tool_call(ptr, len) };
    let out_ptr = (packed >> 32) as u32;
    let out_len = (packed & 0xffff_ffff) as u32;
    let response = read_region(out_ptr, out_len);
    let envelope: serde_json::Value = serde_json::from_slice(response)
        .map_err(|e| format!("tool host returned non-JSON: {e}"))?;
    if let Some(err) = envelope.get("err").and_then(serde_json::Value::as_str) {
        return Err(err.to_owned());
    }
    envelope
        .get("ok")
        .cloned()
        .ok_or_else(|| "tool host envelope has neither ok nor err".to_owned())
}

/// Pure build: a tool stage in the payload is an honest trap.
#[cfg(not(feature = "tools"))]
fn host_tool(name: &str, _input: &serde_json::Value) -> Result<serde_json::Value, String> {
    Err(format!(
        "tool {name:?} requested but this is the pure interpreter build          (zero imports); capability artifacts embed the tools build"
    ))
}

// the payload is either a single program (span artifacts) or a pipeline
// (region artifacts, spec/synthesis.md §8) — one loaded form, one evaluator
static PAYLOAD: OnceLock<Payload> = OnceLock::new();

/// Bump allocator for host → module byte transfer. Leaked on purpose:
/// instances are single-shot (one init + one run), so nothing is ever freed.
// unsafe(no_mangle) justification: the frozen ABI names this export; the
// crate is a cdylib with exactly one definition per symbol
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: u32) -> u32 {
    let buffer = vec![0u8; len as usize].into_boxed_slice();
    // leak: the region must stay valid for the host to write into; the
    // instance is discarded after one call, reclaiming everything
    Box::leak(buffer).as_ptr() as u32
}

/// Load the program (canonical DSL json). Called once per instance, before
/// `run`. Panics (traps) on malformed programs or a second call.
// unsafe(no_mangle) justification: frozen ABI export name (init extension)
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn init(ptr: u32, len: u32) {
    let bytes = read_region(ptr, len);
    let text = std::str::from_utf8(bytes).expect("program bytes must be utf-8");
    let payload = match Payload::from_json(text) {
        Ok(payload) => payload,
        Err(error) => panic!("program.json must parse: {error}"),
    };
    PAYLOAD
        .set(payload)
        .expect("init must be called exactly once per instance");
}

/// Evaluate the loaded program on canonical-JSON input bytes; returns
/// `((out_ptr as u64) << 32) | out_len` per the frozen ABI.
// unsafe(no_mangle) justification: frozen ABI export name
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn run(ptr: u32, len: u32) -> u64 {
    let payload = PAYLOAD
        .get()
        .expect("run requires init to have loaded a program");
    let bytes = read_region(ptr, len);
    let text = std::str::from_utf8(bytes).expect("input bytes must be utf-8");
    let input: serde_json::Value = serde_json::from_str(text).expect("input must be JSON");
    // not code evaluation: auto_dsl::eval_payload interprets a closed enum of
    // pure data transformations (no I/O, no exec) — see auto-dsl docs
    let mut seam = |name: &str, value: &serde_json::Value| host_tool(name, value);
    let output = match auto_dsl::eval_payload(payload, &input, &mut seam) {
        Ok(output) => output,
        Err(error) => panic!("payload evaluation failed: {error}"),
    };
    // canonical: serde_json's map is ordered (no preserve_order anywhere)
    let out = serde_json::to_string(&output)
        .expect("output serialization cannot fail")
        .into_bytes()
        .into_boxed_slice();
    let out_len = u32::try_from(out.len()).expect("output fits u32");
    // leak: the region must outlive this call for the host to read
    let out_ptr = Box::leak(out).as_ptr() as u32;
    (u64::from(out_ptr) << 32) | u64::from(out_len)
}

/// View a host-written region. Confined by construction: on wasm32 every
/// address is inside this module's linear memory, and the host bounds-checks
/// the region it wrote.
#[allow(unsafe_code)] // raw-pointer view of caller-provided (ptr, len); wasm32 linear memory
fn read_region(ptr: u32, len: u32) -> &'static [u8] {
    // SAFETY: (ptr, len) designate bytes inside this module's own linear
    // memory, written by the host immediately before the call; the memory is
    // never shrunk and the instance is single-shot, so the region stays
    // valid for the 'static view an extern ABI function needs.
    unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) }
}
