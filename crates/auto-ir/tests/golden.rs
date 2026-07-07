//! Golden files pin the canonical encoding (`.air`) and the human rendering
//! (`.txt`) of three reference graphs. Bless changes with `UPDATE_GOLDEN=1`.
//!
//! If the byte goldens drift, that is a wire-format change: it needs a
//! schema-version decision, not a casual re-bless (spec/ir.md "versioning").

use std::fs;
use std::path::PathBuf;

use auto_ir::{Graph, examples, from_bytes, render, to_bytes};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn check(name: &str, graph: &Graph) {
    let bytes = to_bytes(graph).expect("serialize");
    let text = render(graph);
    let air = golden_dir().join(format!("{name}.air"));
    let txt = golden_dir().join(format!("{name}.txt"));

    if std::env::var_os("UPDATE_GOLDEN").is_some_and(|v| v == "1") {
        fs::create_dir_all(golden_dir()).expect("create golden dir");
        fs::write(&air, &bytes).expect("write golden bytes");
        fs::write(&txt, &text).expect("write golden text");
    }

    let want_bytes = fs::read(&air).unwrap_or_else(|e| {
        panic!(
            "missing golden {}: {e}\nbless with: UPDATE_GOLDEN=1 cargo test -p auto-ir --test golden",
            air.display()
        )
    });
    assert_eq!(
        bytes, want_bytes,
        "{name}: serialized bytes drifted from golden — wire-format change"
    );

    let want_text = fs::read_to_string(&txt).expect("read golden text");
    assert_eq!(text, want_text, "{name}: rendering drifted from golden");

    // committed bytes decode to the same value and re-serialize byte-identically
    let decoded = from_bytes(&want_bytes).expect("golden bytes decode");
    assert_eq!(&decoded, graph, "{name}: golden decode mismatch");
    assert_eq!(
        to_bytes(&decoded).expect("re-serialize"),
        want_bytes,
        "{name}: golden round-trip not byte-stable"
    );
}

#[test]
fn golden_tool_chain() {
    check("tool_chain", &examples::tool_chain());
}

#[test]
fn golden_branching() {
    check("branching", &examples::branching());
}

#[test]
fn golden_generative_effects() {
    check("generative_effects", &examples::generative_effects());
}
