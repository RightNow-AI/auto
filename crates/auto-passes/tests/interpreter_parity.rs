//! Differential parity: the embedded wasm interpreter
//! ([`auto_passes::interpreter_wasm`]) against the native evaluator
//! (`auto_dsl::eval`) — one implementation, two compilations, byte-equal
//! outputs. Also proves the physical purity claim (zero imports) and that
//! every interpreter failure path traps instead of answering.
//!
//! Host driving mirrors the frozen ABI (spec/artifact.md §4) plus the `init`
//! extension: alloc → write program.json → init → alloc → write canonical
//! input → run → unpack `(out_ptr << 32) | out_len` → bounds-check → read.
//! Plain engine, no fuel: DSL programs are straight-line, so every test call
//! terminates by construction.

use std::sync::OnceLock;

use auto_passes::auto_dsl::{self, Op, Program};
use serde_json::{Value, json};
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

/// Compile the embedded module once; every test drives fresh instances of it
/// (the ABI is one `init` + one `run` per instance).
fn shared_module() -> &'static (Engine, Module) {
    static MODULE: OnceLock<(Engine, Module)> = OnceLock::new();
    MODULE.get_or_init(|| {
        let engine = Engine::default();
        let module = Module::new(&engine, auto_passes::interpreter_wasm())
            .expect("embedded interpreter compiles");
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

    /// Load `program.json` bytes. `Err` is a trap.
    fn init(&mut self, program_json: &str) -> wasmtime::Result<()> {
        let (ptr, len) = self.stage(program_json.as_bytes());
        self.init.call(&mut self.store, (ptr, len))
    }

    /// Evaluate canonical-JSON input bytes; `Ok` is the exact output bytes
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

/// Drive a fresh instance through init + run; return the raw output bytes.
fn interpret(program: &Program, input: &Value) -> Vec<u8> {
    let mut interp = Interp::new();
    interp
        .init(&program.to_json())
        .expect("canonical program json initializes");
    let input_json = serde_json::to_string(input).expect("value serialization cannot fail");
    interp
        .run(&input_json)
        .expect("run does not trap on a well-typed case")
}

/// The parity claim: the wasm interpreter's output bytes equal the native
/// evaluator's canonical serialization, byte for byte.
fn assert_parity(program: &Program, input: &Value) {
    // not code evaluation: auto_dsl::eval interprets a closed enum of pure
    // data transformations (no I/O, no exec) — see auto-dsl docs
    let native = auto_dsl::eval(program, input).expect("native eval succeeds");
    let native_bytes = serde_json::to_string(&native).expect("value serialization cannot fail");
    let wasm_bytes = interpret(program, input);
    assert_eq!(
        wasm_bytes.as_slice(),
        native_bytes.as_bytes(),
        "wasm {:?} != native {native_bytes:?}",
        String::from_utf8_lossy(&wasm_bytes)
    );
}

/// The physical purity claim: the module compiles and imports nothing, so it
/// cannot reach the host at all (spec/artifact.md §5).
#[test]
fn module_compiles_with_zero_imports() {
    let engine = Engine::default();
    let module = Module::new(&engine, auto_passes::interpreter_wasm())
        .expect("embedded interpreter compiles");
    assert_eq!(
        module.imports().len(),
        0,
        "v0 artifacts are pure: zero imports"
    );
}

/// The S4 target pipeline (the fake-frontier extraction) on the recorded doc.
#[test]
fn parity_fake_frontier_pipeline() {
    let program = Program::new(vec![
        Op::GetField {
            key: "prompt".into(),
        },
        Op::Lowercase,
        Op::SplitWhitespace,
        Op::TrimEachMatches { set: ".,".into() },
        Op::FilterLongerThan { n: 4 },
        Op::DedupSort,
        Op::Take { n: 3 },
        Op::Join { sep: " ".into() },
    ]);
    let input =
        json!({"prompt": "The quick brown fox jumps over the lazy dog near the riverbank."});
    assert_eq!(
        auto_dsl::eval(&program, &input),
        Ok(json!("brown jumps quick")),
        "native behavior pinned to the recorded output"
    );
    assert_parity(&program, &input);
}

#[test]
fn parity_wordcount_pipeline() {
    let program = Program::new(vec![
        Op::GetField { key: "text".into() },
        Op::SplitWhitespace,
        Op::Count,
    ]);
    let input = json!({"text": "a b c"});
    assert_eq!(auto_dsl::eval(&program, &input), Ok(json!(3)));
    assert_parity(&program, &input);
}

/// `ConstOut` ignores the register; the constant round-trips through
/// program.json and back out byte-identically (sorted keys on both sides).
#[test]
fn parity_const_out_on_arbitrary_input() {
    let program = Program::new(vec![Op::ConstOut {
        value: json!({"z": 1, "a": [true, null], "emoji": "🦀"}),
    }]);
    assert_parity(&program, &json!({"whatever": [1, {"nested": "🎉"}]}));
}

/// Char semantics across the two compilations: accents, emoji, and U+0130
/// (İ), whose lowercase is two chars (i + U+0307). `CharCount` counts chars,
/// not utf-8 bytes: 31 input chars lowercase to 32.
#[test]
fn parity_unicode_lowercase_char_count() {
    let program = Program::new(vec![Op::Lowercase, Op::CharCount]);
    let input = json!("Grüße, CAFÉ ÉCLAIR! 🦀🎉 İstanbul");
    assert_eq!(auto_dsl::eval(&program, &input), Ok(json!(32)));
    assert_parity(&program, &input);
}

// trap paths: every interpreter failure is a panic in the module, which
// reaches the host as a trap (Err) — never an in-band answer

#[test]
fn run_before_init_traps() {
    let mut interp = Interp::new();
    assert!(
        interp.run("{}").is_err(),
        "run without a loaded program must trap"
    );
}

#[test]
fn init_with_invalid_program_traps() {
    assert!(
        Interp::new().init("not json").is_err(),
        "malformed program bytes must trap"
    );
    assert!(
        Interp::new()
            .init(r#"{"dsl_version":0,"ops":["warp_speed"]}"#)
            .is_err(),
        "strict parse: an unknown op must trap"
    );
}

#[test]
fn second_init_traps() {
    let program_json = Program::new(vec![Op::Trim]).to_json();
    let mut interp = Interp::new();
    interp.init(&program_json).expect("first init succeeds");
    assert!(
        interp.init(&program_json).is_err(),
        "init is once per instance"
    );
}

#[test]
fn eval_type_error_traps() {
    let program = Program::new(vec![Op::Lowercase]);
    assert!(
        auto_dsl::eval(&program, &json!(42)).is_err(),
        "native side type-errors on the same case"
    );
    let mut interp = Interp::new();
    interp.init(&program.to_json()).expect("init succeeds");
    assert!(
        interp.run("42").is_err(),
        "lowercase on a number must trap, matching the native error"
    );
}
