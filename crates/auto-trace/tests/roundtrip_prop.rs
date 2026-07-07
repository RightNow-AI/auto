//! Property tests: arbitrary valid traces survive the full pipeline —
//! JSONL emission → strict parse → sqlite ingest → load — losslessly.

use std::collections::BTreeMap;

use auto_trace::jsonl::{parse_str, to_jsonl};
use auto_trace::model::{Span, SpanId, SpanKind, TaskOutput, Trace, TraceHeader, TraceId};
use auto_trace::store::Store;
use proptest::prelude::*;
use serde_json::Value;

/// Values the store must carry losslessly (i64-representable, finite floats).
fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        // finite doubles only: NaN/inf are not JSON
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(Value::from),
        "[a-zA-Z0-9 _-]{0,12}".prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::from),
            prop::collection::btree_map("[a-z]{1,6}", inner, 0..4)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

fn arb_kind() -> impl Strategy<Value = SpanKind> {
    prop_oneof![
        Just(SpanKind::ModelCall),
        Just(SpanKind::ToolCall),
        Just(SpanKind::EnvRead),
        Just(SpanKind::MemoryOp),
        Just(SpanKind::Branch),
        Just(SpanKind::Span),
    ]
}

fn arb_attrs() -> impl Strategy<Value = BTreeMap<String, String>> {
    prop::collection::btree_map("[a-z.]{1,8}", "[ -~]{0,10}", 0..3)
}

#[derive(Debug, Clone)]
struct SpanRecipe {
    seq_gap: u64,
    parent_pick: Option<u64>,
    kind: SpanKind,
    name: String,
    input: Value,
    output: Option<Value>,
    error: Option<String>,
    duration_ms: u64,
    attrs: BTreeMap<String, String>,
}

fn arb_recipe() -> impl Strategy<Value = SpanRecipe> {
    (
        (
            1u64..50,
            prop::option::of(any::<u64>()),
            arb_kind(),
            "[a-z._]{1,10}",
        ),
        (
            arb_json(),
            prop::option::of(arb_json()),
            prop::option::of("[ -~]{1,20}"),
            0u64..100_000,
            arb_attrs(),
        ),
    )
        .prop_map(
            |((seq_gap, parent_pick, kind, name), (input, output, error, duration_ms, attrs))| {
                SpanRecipe {
                    seq_gap,
                    parent_pick,
                    kind,
                    name,
                    input,
                    output,
                    error,
                    duration_ms,
                    attrs,
                }
            },
        )
}

/// Optional task-level I/O (ADR-0025). The wire reads a `null` task input as
/// absent, so the model's value space excludes `Some(Null)` for inputs; a
/// task output may be any JSON including null — the presence of its line is
/// what distinguishes "declared null" from "never declared".
fn arb_task_io() -> impl Strategy<Value = (Option<Value>, Option<TaskOutput>)> {
    (
        prop::option::of(
            arb_json().prop_filter("null task input reads as absent", |v| !v.is_null()),
        ),
        prop::option::of((arb_json(), 0u64..(i64::MAX as u64 / 2))),
    )
        .prop_map(|(task_input, out)| {
            (
                task_input,
                out.map(|(value, recorded_at_ms)| TaskOutput {
                    value,
                    recorded_at_ms,
                }),
            )
        })
}

fn build_trace(id: u128, task: String, started_at_ms: u64, recipes: Vec<SpanRecipe>) -> Trace {
    let mut spans: Vec<Span> = Vec::with_capacity(recipes.len());
    let mut seq = 0u64;
    for (i, r) in recipes.into_iter().enumerate() {
        seq += r.seq_gap; // strictly increasing
        let parent_span_id = r.parent_pick.and_then(|p| {
            if i == 0 {
                None
            } else {
                // any earlier span; earlier index ⇒ smaller seq
                let idx = usize::try_from(p % i as u64).expect("index fits");
                Some(spans[idx].span_id)
            }
        });
        spans.push(Span {
            span_id: SpanId(u64::try_from(i).expect("fits") + 1),
            parent_span_id,
            seq,
            kind: r.kind,
            name: r.name,
            input: r.input,
            // the wire conflates Some(Null) with None (spec/trace.md); the
            // model's value space has one representative — absent
            output: r.output.filter(|v| !v.is_null()),
            error: r.error,
            started_at_ms: started_at_ms.saturating_add(seq),
            duration_ms: r.duration_ms,
            attrs: r.attrs,
        });
    }
    Trace {
        header: TraceHeader {
            trace_id: TraceId(id),
            task,
            started_at_ms,
            sdk: "prop/0".into(),
            attrs: BTreeMap::new(),
            task_input: None,
            task_output: None,
        },
        spans,
    }
}

fn arb_trace() -> impl Strategy<Value = Trace> {
    (
        any::<u128>(),
        "[a-z-]{1,10}",
        // i64-representable epoch millis with headroom for seq offsets
        0u64..(i64::MAX as u64 / 2),
        prop::collection::vec(arb_recipe(), 0..10),
        arb_task_io(),
    )
        .prop_map(|(id, task, started, recipes, (task_input, task_output))| {
            let mut trace = build_trace(id, task, started, recipes);
            trace.header.task_input = task_input;
            trace.header.task_output = task_output;
            trace
        })
}

proptest! {
    #[test]
    fn jsonl_roundtrip_is_lossless(t in arb_trace()) {
        let text = to_jsonl(&t);
        let parsed = parse_str(&text).expect("emitted jsonl parses");
        prop_assert_eq!(&parsed, &t);
    }

    #[test]
    fn store_roundtrip_is_lossless(t in arb_trace()) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(&dir.path().join("p.db")).expect("open");
        store.ingest(&t).expect("ingest valid trace");
        let loaded = store.load_trace(t.header.trace_id).expect("load");
        prop_assert_eq!(&loaded, &t);
    }
}
