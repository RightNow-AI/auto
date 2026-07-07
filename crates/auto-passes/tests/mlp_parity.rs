//! Differential parity: the embedded MLP interpreter
//! ([`auto_passes::mlp_interpreter_wasm`]) against native inference
//! (`auto_model::infer_mlp`) — one implementation, two compilations,
//! byte-equal outputs. Also proves the physical purity claim (zero imports)
//! and that every inference failure path traps instead of answering.
//!
//! Host driving mirrors the frozen ABI (spec/artifact.md §4) plus the `init`
//! extension — for neural distilled artifacts the init payload is MLP json:
//! alloc → write mlp.json → init → alloc → write canonical input → run →
//! unpack `(out_ptr << 32) | out_len` → bounds-check → read. Plain engine,
//! no fuel: inference is one relu matmul plus an argmax, so every test call
//! terminates.

use std::sync::OnceLock;

use auto_passes::auto_model::{self, Features, Mlp, MlpError, fnv1a_32};
use serde_json::{Value, json};
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

/// Compile the embedded module once; every test drives fresh instances of it
/// (the ABI is one `init` + one `run` per instance).
fn shared_module() -> &'static (Engine, Module) {
    static MODULE: OnceLock<(Engine, Module)> = OnceLock::new();
    MODULE.get_or_init(|| {
        let engine = Engine::default();
        let module = Module::new(&engine, auto_passes::mlp_interpreter_wasm())
            .expect("embedded mlp interpreter compiles");
        (engine, module)
    })
}

/// One fresh interpreter instance, driven through the frozen ABI + `init`.
struct Interp {
    store: Store<()>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    init: TypedFunc<(u32, u32), ()>,
    run: TypedFunc<(u32, u32), u64>,
}

impl Interp {
    fn new() -> Self {
        let (engine, module) = shared_module();
        let mut store = Store::new(engine, ());
        let instance =
            Instance::new(&mut store, module, &[]).expect("zero-import module instantiates");
        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("module exports `memory`");
        let alloc = instance
            .get_typed_func::<u32, u32>(&mut store, "alloc")
            .expect("module exports `alloc: (i32) -> i32`");
        let init = instance
            .get_typed_func::<(u32, u32), ()>(&mut store, "init")
            .expect("module exports `init: (i32, i32) -> ()`");
        let run = instance
            .get_typed_func::<(u32, u32), u64>(&mut store, "run")
            .expect("module exports `run: (i32, i32) -> i64`");
        Self {
            store,
            memory,
            alloc,
            init,
            run,
        }
    }

    /// `alloc` a region, bounds-check it against the memory size, write
    /// `bytes` into it.
    fn stage(&mut self, bytes: &[u8]) -> (u32, u32) {
        let len = u32::try_from(bytes.len()).expect("test payloads fit u32");
        let ptr = self
            .alloc
            .call(&mut self.store, len)
            .expect("alloc does not trap");
        let mem_size = self.memory.data_size(&self.store) as u64;
        assert!(
            u64::from(ptr) + u64::from(len) <= mem_size,
            "alloc({len}) returned ptr={ptr}, past memory size {mem_size}"
        );
        self.memory
            .write(&mut self.store, ptr as usize, bytes)
            .expect("write stays inside the checked region");
        (ptr, len)
    }

    /// Load mlp json (the neural artifact's init payload). `Err` is a trap.
    fn init(&mut self, mlp_json: &str) -> wasmtime::Result<()> {
        let (ptr, len) = self.stage(mlp_json.as_bytes());
        self.init.call(&mut self.store, (ptr, len))
    }

    /// Infer on canonical-JSON input bytes; `Ok` is the exact output bytes
    /// read from the packed `(out_ptr << 32) | out_len` region, bounds-checked
    /// against the (possibly grown) memory. `Err` is a trap.
    fn run(&mut self, input_json: &str) -> wasmtime::Result<Vec<u8>> {
        let (ptr, len) = self.stage(input_json.as_bytes());
        let packed = self.run.call(&mut self.store, (ptr, len))?;
        let out_ptr = u32::try_from(packed >> 32).expect("high 32 bits fit u32");
        let out_len = u32::try_from(packed & 0xffff_ffff).expect("low 32 bits fit u32");
        // re-read the size: run may have grown memory
        let mem_size = self.memory.data_size(&self.store) as u64;
        assert!(
            u64::from(out_ptr) + u64::from(out_len) <= mem_size,
            "run returned region ptr={out_ptr} len={out_len}, past memory size {mem_size}"
        );
        let mut out = vec![0u8; out_len as usize];
        self.memory
            .read(&self.store, out_ptr as usize, &mut out)
            .expect("read stays inside the checked region");
        Ok(out)
    }
}

/// Drive a fresh instance through init(mlp json) + run; raw output bytes.
fn infer_wasm(mlp: &Mlp, input: &Value) -> Vec<u8> {
    let mut interp = Interp::new();
    interp
        .init(&mlp.to_json())
        .expect("canonical mlp json initializes");
    let input_json = serde_json::to_string(input).expect("value serialization cannot fail");
    interp
        .run(&input_json)
        .expect("run does not trap on a well-typed case")
}

/// The parity claim: the wasm interpreter's output bytes equal the native
/// inference's canonical serialization, byte for byte.
fn assert_parity(mlp: &Mlp, input: &Value) {
    // not code evaluation: auto_model::infer_mlp is a counted-trigram
    // featurize plus one relu matmul and an argmax — pure data, no I/O
    let native = auto_model::infer_mlp(mlp, input).expect("native inference succeeds");
    let native_bytes = serde_json::to_string(&native).expect("value serialization cannot fail");
    let wasm_bytes = infer_wasm(mlp, input);
    assert_eq!(
        wasm_bytes.as_slice(),
        native_bytes.as_bytes(),
        "wasm {:?} != native {native_bytes:?}",
        String::from_utf8_lossy(&wasm_bytes)
    );
}

/// Feature index of a trigram under the frozen spec: fnv1a-32 % buckets.
fn bucket(trigram: &str, buckets: u32) -> u32 {
    fnv1a_32(trigram.as_bytes()) % buckets
}

/// A weight row that is 1.0 at `index` and 0.0 elsewhere: the hidden unit
/// becomes the occurrence count of one trigram bucket — hand-computable.
fn one_hot(index: u32, width: u32) -> Vec<f64> {
    let mut row = vec![0.0; width as usize];
    row[index as usize] = 1.0;
    row
}

const BUCKETS: u32 = 512;

/// Hand-built MLP over object inputs (`input_field = "text"`), one hidden
/// unit counting the trigram "foo": logits = [0.5 - h, h], so "foo" absent
/// (h = 0) answers "miss" and present (h >= 1) answers "hit".
fn fielded_mlp() -> Mlp {
    Mlp {
        features: Features {
            kind: "char_trigram_fnv1a".into(),
            buckets: BUCKETS,
            input_field: Some("text".into()),
        },
        hidden_weights: vec![one_hot(bucket("foo", BUCKETS), BUCKETS)],
        hidden_bias: vec![0.0],
        out_weights: vec![vec![-1.0], vec![1.0]],
        out_bias: vec![0.5, 0.0],
        classes: vec!["miss".into(), "hit".into()],
    }
}

/// Hand-built MLP over bare-string inputs (`input_field = None`), two hidden
/// units (occurrence counts of "the" and "caf") and three classes: "has-the"
/// at 10·h₀, "cafe" at 2·h₁, "other" winning the empty forward pass through
/// its 0.5 bias.
fn bare_mlp() -> Mlp {
    Mlp {
        features: Features {
            kind: "char_trigram_fnv1a".into(),
            buckets: BUCKETS,
            input_field: None,
        },
        hidden_weights: vec![
            one_hot(bucket("the", BUCKETS), BUCKETS),
            one_hot(bucket("caf", BUCKETS), BUCKETS),
        ],
        hidden_bias: vec![0.0, 0.0],
        out_weights: vec![vec![10.0, 0.0], vec![0.0, 2.0], vec![0.0, 0.0]],
        out_bias: vec![0.0, 0.0, 0.5],
        classes: vec!["has-the".into(), "cafe".into(), "other".into()],
    }
}

/// The pinned FNV-1a vectors (frozen feature spec). Parity would be
/// meaningless against a drifted hash, so pin it here too.
#[test]
fn fnv1a_pinned_vectors() {
    assert_eq!(fnv1a_32(b""), 2_166_136_261);
    assert_eq!(fnv1a_32(b"a"), 0xE40C_292C);
    assert_eq!(fnv1a_32(b"abc"), 0x1A47_E90B);
}

/// The physical purity claim: the module compiles and imports nothing, so it
/// cannot reach the host at all (spec/artifact.md §5).
#[test]
fn module_compiles_with_zero_imports() {
    let engine = Engine::default();
    let module = Module::new(&engine, auto_passes::mlp_interpreter_wasm())
        .expect("embedded mlp interpreter compiles");
    assert_eq!(
        module.imports().len(),
        0,
        "v0 artifacts are pure: zero imports"
    );
}

/// Parity, MLP with `input_field`: three inputs (a hit, a miss, and a
/// unicode text whose lowercasing must agree across the two compilations).
#[test]
fn parity_fielded_mlp_three_inputs() {
    let mlp = fielded_mlp();
    for (input, expected) in [
        (json!({"text": "xx foo xx"}), json!("hit")),
        (json!({"text": "nothing to see"}), json!("miss")),
        (json!({"text": "Grüße FOO 🦀 İstanbul"}), json!("hit")),
    ] {
        assert_eq!(
            auto_model::infer_mlp(&mlp, &input),
            Ok(expected),
            "native behavior pinned for {input}"
        );
        assert_parity(&mlp, &input);
    }
}

/// Parity, MLP without `input_field` (the input IS the text): three bare
/// strings — one per class, including the empty string (no trigrams: the
/// forward pass is bias-only) and a unicode input (İ lowercases to two
/// chars).
#[test]
fn parity_bare_mlp_three_inputs() {
    let mlp = bare_mlp();
    for (input, expected) in [
        (json!("over the lazy dog"), json!("has-the")),
        (json!(""), json!("other")),
        (json!("İstanbul CAFÉ visit"), json!("cafe")),
    ] {
        assert_eq!(
            auto_model::infer_mlp(&mlp, &input),
            Ok(expected),
            "native behavior pinned for {input}"
        );
        assert_parity(&mlp, &input);
    }
}

/// Argmax ties break toward the LOWEST class index — a documented wire-level
/// convention, so it must hold identically in both compilations.
#[test]
fn parity_tie_breaks_low_index() {
    let mlp = Mlp {
        features: Features {
            kind: "char_trigram_fnv1a".into(),
            buckets: BUCKETS,
            input_field: None,
        },
        hidden_weights: vec![vec![0.0; BUCKETS as usize]],
        hidden_bias: vec![0.0],
        out_weights: vec![vec![0.0], vec![0.0]],
        out_bias: vec![0.5, 0.5],
        classes: vec!["first".into(), "second".into()],
    };
    let input = json!("every class scores 0.5 here");
    assert_eq!(
        auto_model::infer_mlp(&mlp, &input),
        Ok(json!("first")),
        "native ties break toward the lowest index"
    );
    assert_parity(&mlp, &input);
}

// trap paths: every interpreter failure is a panic in the module, which
// reaches the host as a trap (Err) — never an in-band answer

#[test]
fn run_before_init_traps() {
    let mut interp = Interp::new();
    assert!(
        interp.run("\"abc\"").is_err(),
        "run without a loaded mlp must trap"
    );
}

#[test]
fn init_with_invalid_mlp_traps() {
    assert!(
        Interp::new().init("not json").is_err(),
        "malformed mlp bytes must trap"
    );
    // structurally invalid: this build reads mlp_version 0 exactly
    let future = fielded_mlp()
        .to_json()
        .replace("\"mlp_version\":0", "\"mlp_version\":1");
    assert!(
        Interp::new().init(&future).is_err(),
        "unsupported mlp_version must trap"
    );
}

#[test]
fn init_with_shape_mismatch_traps() {
    // well-formed json, wrong shapes: hidden_bias has 2 entries against 1
    // hidden row — the native reader calls it BadShape, the wasm must trap
    let bad = r#"{"classes":["a","b"],"features":{"buckets":2,"kind":"char_trigram_fnv1a"},"hidden_bias":[0.0,0.0],"hidden_weights":[[1.0,-1.0]],"mlp_version":0,"out_bias":[0.0,0.1],"out_weights":[[1.0],[-1.0]]}"#;
    assert!(
        matches!(Mlp::from_json(bad), Err(MlpError::BadShape(_))),
        "native side rejects the same json as a shape mismatch"
    );
    assert!(
        Interp::new().init(bad).is_err(),
        "shape-mismatch mlp json must trap"
    );
}

#[test]
fn inference_error_traps() {
    // fielded mlp, input missing the field: native errors, wasm traps
    let mlp = fielded_mlp();
    assert!(
        auto_model::infer_mlp(&mlp, &json!({"other": "foo"})).is_err(),
        "native side errors on the same case"
    );
    let mut interp = Interp::new();
    interp.init(&mlp.to_json()).expect("init succeeds");
    assert!(
        interp.run("{\"other\":\"foo\"}").is_err(),
        "missing input_field must trap, matching the native error"
    );
}

#[test]
fn second_init_traps() {
    let mlp_json = fielded_mlp().to_json();
    let mut interp = Interp::new();
    interp.init(&mlp_json).expect("first init succeeds");
    assert!(interp.init(&mlp_json).is_err(), "init is once per instance");
}
