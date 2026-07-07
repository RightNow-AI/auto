//! Hand-assisted S3 implementation of the toy agent's `fake-frontier` span.
//!
//! This is the module that goes inside the first `.cbin`. The S3 spine item
//! allows hand-assisted passes, so a human wrote this crate; automated
//! symbolic extraction lands S4. It implements the frozen module ABI
//! (spec/artifact.md §4) for the mapping the toy agent records:
//! input `{"prompt": <text>}` → output `<text>`, the same keyword extraction
//! as `fake_model()` in `evals/toy-agent/agent.py` (lowercase, split on
//! whitespace, strip leading/trailing `.`/`,`, keep words longer than 4
//! chars, dedupe, sort, take 3, join with spaces).
//!
//! # malformed input
//!
//! The contract's interface is input `json`, output `text` — so an input
//! without a string `"prompt"` key is reachable at the ABI boundary. This
//! implementation panics on such input; on the wasm target a panic is a
//! trap, and the host reports a trap as an execution failure. That is the
//! honest failure mode: no invented `""` output, no silent recovery.
//!
//! # feature `wrong`
//!
//! Deliberately mis-implements the mapping: keeps 2 keywords instead of 3.
//! It exists so the e2e can prove the emit gate blocks an implementation
//! whose outputs diverge from the recorded observations. Never ship it.
//!
//! # fidelity bounds
//!
//! Faithful to `fake_model()` over the recorded input domain (ascii). Exotic
//! unicode edges (python's extra control-char whitespace in `str.split`,
//! locale-free case folding differences) are not part of any recorded
//! observation and are not claimed.

use std::collections::BTreeSet;

/// Keywords kept. The `wrong` feature deliberately diverges (see crate docs).
#[cfg(not(feature = "wrong"))]
const KEEP: usize = 3;
#[cfg(feature = "wrong")]
const KEEP: usize = 2;

/// The extraction `fake_model()` performs: lowercase, split on whitespace,
/// strip leading/trailing `.` and `,` from each word (python `str.strip(".,")`
/// semantics), keep words with more than 4 chars (`chars().count()`), dedupe
/// and sort (`BTreeSet` byte order == python lexicographic for ascii), take
/// the first [`KEEP`], join with single spaces.
pub fn extract_keywords(prompt: &str) -> String {
    let lowered = prompt.to_lowercase();
    let keywords: BTreeSet<String> = lowered
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '.' || c == ','))
        .filter(|w| w.chars().count() > 4)
        .map(str::to_owned)
        .collect();
    keywords
        .into_iter()
        .take(KEEP)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Full input→output mapping over ABI byte payloads: canonical-JSON input
/// bytes in, canonical-JSON output bytes out (the output value is a JSON
/// string, so `serde_json::to_string` of a `String` is already canonical).
///
/// # Panics
///
/// On input that is not a JSON object with a string `"prompt"` key. On the
/// wasm target the panic traps; the host reports an execution failure.
pub fn respond(input: &[u8]) -> Vec<u8> {
    let value: serde_json::Value =
        serde_json::from_slice(input).expect("fake-frontier input is not valid JSON");
    let prompt = value
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .expect("fake-frontier input has no string \"prompt\" key");
    let output = extract_keywords(prompt);
    serde_json::to_string(&output)
        .expect("serializing a string cannot fail")
        .into_bytes()
}

/// The frozen module ABI (spec/artifact.md §4): zero imports; exports
/// `memory`, `alloc`, `run`. Compiled only for the wasm target — the native
/// build exists to unit-test the mapping above.
#[cfg(target_arch = "wasm32")]
// justification: the ABI is raw by construction — the host writes input bytes
// into our linear memory at a (ptr, len) it obtained from `alloc` and reads
// output bytes back from a packed (ptr, len); no safe wrapper can sit below
// the export boundary itself. `#[unsafe(no_mangle)]` is required for the
// fixed export names the loader looks up.
#[allow(unsafe_code)]
mod abi {
    /// Allocate `len` zeroed bytes in linear memory and hand the pointer to
    /// the host, which writes the input there before calling [`run`].
    #[unsafe(no_mangle)]
    pub extern "C" fn alloc(len: u32) -> u32 {
        let buf = vec![0u8; len as usize].into_boxed_slice();
        // leaked deliberately: the buffer must outlive this call so the host
        // can fill it; the host instantiates fresh per `run` call, so every
        // leak dies with the instance.
        Box::leak(buf).as_mut_ptr() as u32
    }

    /// Read `in_len` input bytes at `in_ptr`, compute, and pack the output
    /// location as `((out_ptr as u64) << 32) | (out_len as u64)` (bit-cast to
    /// wasm `i64` at the boundary).
    #[unsafe(no_mangle)]
    pub extern "C" fn run(in_ptr: u32, in_len: u32) -> u64 {
        // SAFETY: per the ABI the host wrote exactly `in_len` bytes at
        // `in_ptr` — a buffer it obtained from `alloc` in this instance's
        // linear memory — before the call, and does not touch that memory
        // during it. The range is valid, initialized, and unaliased for the
        // lifetime of this borrow.
        let input = unsafe { core::slice::from_raw_parts(in_ptr as *const u8, in_len as usize) };
        let out = super::respond(input);
        let out_len = out.len() as u32;
        // leaked deliberately, same rule as `alloc`: the host reads the
        // output after `run` returns; the instance is torn down afterwards.
        let out_ptr = Box::leak(out.into_boxed_slice()).as_ptr() as u32;
        (u64::from(out_ptr) << 32) | u64::from(out_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact recorded observation the contract example pins
    /// (evals/toy-agent/fake-frontier.contract.toml, "riverbank-doc").
    const RECORDED_PROMPT: &str = "The quick brown fox jumps over the lazy dog near the riverbank.";

    #[cfg(not(feature = "wrong"))]
    #[test]
    fn matches_recorded_output() {
        assert_eq!(extract_keywords(RECORDED_PROMPT), "brown jumps quick");
    }

    #[cfg(not(feature = "wrong"))]
    #[test]
    fn respond_emits_canonical_json_string_bytes() {
        let input = format!(
            "{{\"prompt\":{}}}",
            serde_json::to_string(RECORDED_PROMPT).unwrap()
        );
        assert_eq!(
            respond(input.as_bytes()),
            br#""brown jumps quick""#.as_slice()
        );
    }

    #[cfg(feature = "wrong")]
    #[test]
    fn wrong_variant_takes_two() {
        assert_eq!(extract_keywords(RECORDED_PROMPT), "brown jumps");
    }

    #[test]
    fn strips_only_leading_and_trailing_punctuation() {
        // python: "..alpha,beta,.".strip(".,") == "alpha,beta"
        assert_eq!(extract_keywords("..alpha,beta,."), "alpha,beta");
    }

    #[test]
    fn dedupes_and_sorts() {
        assert_eq!(extract_keywords("zebra zebra apple, apple."), "apple zebra");
    }

    #[test]
    fn short_words_only_is_empty_output() {
        // faithful to fake_model(): no keyword survives, the output is "".
        assert_eq!(extract_keywords("a bb ccc dddd"), "");
    }

    #[test]
    #[should_panic(expected = "not valid JSON")]
    fn garbage_bytes_trap() {
        respond(b"not json");
    }

    #[test]
    #[should_panic(expected = "no string \"prompt\" key")]
    fn missing_prompt_traps() {
        respond(br#"{"query":"hello there world"}"#);
    }

    #[test]
    #[should_panic(expected = "no string \"prompt\" key")]
    fn non_string_prompt_traps() {
        respond(br#"{"prompt":7}"#);
    }
}
