//! Divergence canonicalization (ADR-0018 amendment, wave 7): the majority
//! witness pick a trainer opts into explicitly, and the per-output witness
//! counts that survive both gather paths (span and region).

use std::collections::{BTreeMap, BTreeSet};

use auto_backend::differential::{
    Recorded, canonical_pick, gather_observations, gather_region, pick_observations,
    weighted_observations,
};
use auto_contract::{Budgets, Contract, Interface, Scope};
use auto_ir::ValueType;
use auto_trace::Store;
use auto_trace::model::{Span, SpanId, SpanKind, Trace, TraceHeader, TraceId, canonical_json};
use serde_json::{Value, json};

fn span(seq: u64, name: &str, input: Value, output: Option<Value>, error: Option<String>) -> Span {
    Span {
        span_id: SpanId(seq),
        parent_span_id: None,
        seq,
        kind: SpanKind::ModelCall,
        name: name.into(),
        input,
        output,
        error,
        started_at_ms: 0,
        duration_ms: 1,
        attrs: BTreeMap::new(),
    }
}

fn trace(id: u128, spans: Vec<Span>) -> Trace {
    Trace {
        header: TraceHeader {
            trace_id: TraceId(id),
            task: "t".into(),
            started_at_ms: 0,
            sdk: "test/0".into(),
            attrs: BTreeMap::new(),
            task_input: None,
            task_output: None,
        },
        spans,
    }
}

fn store_with(traces: Vec<Trace>) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(&dir.path().join("t.db")).expect("open");
    for t in traces {
        store.ingest(&t).expect("ingest");
    }
    (dir, store)
}

fn contract(scope: Scope) -> Contract {
    Contract {
        acceptance: Default::default(),
        task: "t".into(),
        scope,
        interface: Interface {
            input: ValueType::Json,
            output: ValueType::Text,
        },
        examples: vec![],
        properties: vec![],
        budgets: Budgets::default(),
        eval_cases: vec![],
    }
}

fn span_contract() -> Contract {
    contract(Scope::Span {
        kind: "model_call".into(),
        name: "m".into(),
    })
}

/// A hand-built group: `witnesses` as (output value, count), plus errors.
fn recorded_with(witnesses: &[(Value, usize)], errors: usize) -> Recorded {
    let mut outputs = BTreeSet::new();
    let mut output_counts = BTreeMap::new();
    let mut observations = errors;
    for (output, count) in witnesses {
        let canonical = canonical_json(output);
        outputs.insert(canonical.clone());
        output_counts.insert(canonical, *count);
        observations += count;
    }
    Recorded {
        input: json!({"x": 1}),
        outputs,
        output_counts,
        observations,
        errors,
    }
}

#[test]
fn majority_two_vs_one_picks_the_most_witnessed() {
    // the majority is lexicographically LARGER, so only counts can pick it
    let group = recorded_with(&[(json!("apple"), 1), (json!("zebra"), 2)], 0);
    assert_eq!(canonical_pick(&group), Some(json!("zebra")));
}

#[test]
fn tie_picks_the_lexicographically_smaller_canonical() {
    let group = recorded_with(&[(json!("b"), 1), (json!("a"), 1)], 0);
    assert_eq!(canonical_pick(&group), Some(json!("a")));
}

#[test]
fn unanimous_group_picks_its_output() {
    let group = recorded_with(&[(json!("only"), 3)], 0);
    assert_eq!(canonical_pick(&group), Some(json!("only")));
}

#[test]
fn errored_reference_is_never_trainable() {
    let group = recorded_with(&[(json!("fine"), 2)], 1);
    assert_eq!(canonical_pick(&group), None);
}

#[test]
fn nothing_witnessed_is_no_pick() {
    let group = recorded_with(&[], 0);
    assert_eq!(canonical_pick(&group), None);
}

#[test]
fn pick_observations_skips_and_counts_errored_groups() {
    let (_d, store) = store_with(vec![
        // clean unanimous group
        trace(
            1,
            vec![span(1, "m", json!({"x": 1}), Some(json!("keep")), None)],
        ),
        // errored group: skipped, counted
        trace(
            2,
            vec![span(1, "m", json!({"x": 2}), None, Some("boom".into()))],
        ),
        // divergent group: 2-vs-1 resolves to the majority
        trace(
            3,
            vec![span(1, "m", json!({"x": 3}), Some(json!("z")), None)],
        ),
        trace(
            4,
            vec![span(1, "m", json!({"x": 3}), Some(json!("z")), None)],
        ),
        trace(
            5,
            vec![span(1, "m", json!({"x": 3}), Some(json!("a")), None)],
        ),
    ]);
    let gathered = gather_observations(&store, &span_contract()).expect("gather");
    let (pairs, errored_skipped) = pick_observations(&gathered);
    assert_eq!(errored_skipped, 1);
    assert_eq!(
        pairs,
        vec![
            (json!({"x": 1}), json!("keep")),
            (json!({"x": 3}), json!("z")),
        ],
        "pairs in canonical input order, errored group absent"
    );
}

#[test]
fn weighted_rows_carry_witness_counts_in_canonical_order() {
    // one divergent group: zebra witnessed twice, apple once — one row PER
    // DISTINCT OUTPUT, weight = its witness count, outputs in canonical
    // order ("apple" < "zebra" regardless of counts)
    let (_d, store) = store_with(vec![
        trace(
            1,
            vec![span(1, "m", json!({"x": 1}), Some(json!("zebra")), None)],
        ),
        trace(
            2,
            vec![span(1, "m", json!({"x": 1}), Some(json!("zebra")), None)],
        ),
        trace(
            3,
            vec![span(1, "m", json!({"x": 1}), Some(json!("apple")), None)],
        ),
    ]);
    let gathered = gather_observations(&store, &span_contract()).expect("gather");
    let (rows, errored_skipped) = weighted_observations(&gathered);
    assert_eq!(errored_skipped, 0);
    assert_eq!(
        rows,
        vec![
            (json!({"x": 1}), json!("apple"), 1),
            (json!({"x": 1}), json!("zebra"), 2),
        ]
    );
    // total weight over a clean group's rows == its observation count
    assert_eq!(rows.iter().map(|(_, _, w)| w).sum::<usize>(), 3);
}

#[test]
fn weighted_rows_skip_and_count_errored_groups_in_canonical_input_order() {
    // the same store shape pick_observations is tested on: a unanimous
    // group, an errored group (skipped + counted), a divergent group
    let (_d, store) = store_with(vec![
        trace(
            1,
            vec![span(1, "m", json!({"x": 1}), Some(json!("keep")), None)],
        ),
        trace(
            2,
            vec![span(1, "m", json!({"x": 2}), None, Some("boom".into()))],
        ),
        trace(
            3,
            vec![span(1, "m", json!({"x": 3}), Some(json!("z")), None)],
        ),
        trace(
            4,
            vec![span(1, "m", json!({"x": 3}), Some(json!("z")), None)],
        ),
        trace(
            5,
            vec![span(1, "m", json!({"x": 3}), Some(json!("a")), None)],
        ),
    ]);
    let gathered = gather_observations(&store, &span_contract()).expect("gather");
    let (rows, errored_skipped) = weighted_observations(&gathered);
    assert_eq!(errored_skipped, 1);
    assert_eq!(
        rows,
        vec![
            // unanimous group: a single row, weight = all its witnesses
            (json!({"x": 1}), json!("keep"), 1),
            // divergent group: every witnessed output survives with its count
            (json!({"x": 3}), json!("a"), 1),
            (json!({"x": 3}), json!("z"), 2),
        ],
        "groups in canonical input order, errored group absent, minority witnesses kept"
    );
    // the heaviest row of each group IS the canonical pick (same argmax,
    // same tie rule) — weighting adds the minority rows, it never disagrees
    // with the pick about the majority
    let (pairs, _) = pick_observations(&gathered);
    for (input, pick) in pairs {
        let heaviest = rows
            .iter()
            .filter(|(i, _, _)| *i == input)
            .max_by_key(|(_, output, w)| (*w, std::cmp::Reverse(canonical_json(output))))
            .expect("picked group has rows");
        assert_eq!(heaviest.1, pick);
    }
}

#[test]
fn weighted_rows_survive_region_gather_without_errored_weight() {
    // region path: an errored chain taints its observation — it adds no
    // weight anywhere and its group is skipped only when ALL its chains
    // errored; here the group has clean witnesses AND an error, so the
    // group is errored (never trainable) and contributes nothing
    let chain = |id: u128, out: Value| {
        trace(
            id,
            vec![
                span(1, "a", json!({"q": 1}), Some(json!("mid")), None),
                span(2, "b", json!("mid"), Some(out), None),
            ],
        )
    };
    let errored = trace(
        4,
        vec![
            span(1, "a", json!({"q": 1}), Some(json!("mid")), None),
            span(2, "b", json!("mid"), None, Some("boom".into())),
        ],
    );
    let (_d, store) = store_with(vec![
        chain(1, json!("zebra")),
        chain(2, json!("zebra")),
        chain(3, json!("apple")),
        errored,
    ]);
    let c = contract(Scope::Region {
        from: "a".into(),
        to: "b".into(),
    });
    let region = gather_region(&store, &c).expect("gather region");
    let (rows, errored_skipped) = weighted_observations(&region.gathered);
    assert!(rows.is_empty(), "an errored group is never trainable");
    assert_eq!(errored_skipped, 1);
}

#[test]
fn output_counts_survive_span_gather() {
    let (_d, store) = store_with(vec![
        trace(
            1,
            vec![span(1, "m", json!({"x": 1}), Some(json!("zebra")), None)],
        ),
        trace(
            2,
            vec![span(1, "m", json!({"x": 1}), Some(json!("zebra")), None)],
        ),
        trace(
            3,
            vec![span(1, "m", json!({"x": 1}), Some(json!("apple")), None)],
        ),
    ]);
    let gathered = gather_observations(&store, &span_contract()).expect("gather");
    assert_eq!(gathered.groups.len(), 1);
    let group = gathered.groups.values().next().expect("one group");
    assert_eq!(group.observations, 3);
    assert_eq!(group.errors, 0);
    assert_eq!(group.outputs.len(), 2, "outputs stays a distinct set");
    assert_eq!(
        group.output_counts.get(&canonical_json(&json!("zebra"))),
        Some(&2)
    );
    assert_eq!(
        group.output_counts.get(&canonical_json(&json!("apple"))),
        Some(&1)
    );
    assert_eq!(canonical_pick(group), Some(json!("zebra")));
}

#[test]
fn output_counts_survive_region_gather() {
    let chain = |id: u128, out: Value| {
        trace(
            id,
            vec![
                span(1, "a", json!({"q": 1}), Some(json!("mid")), None),
                span(2, "b", json!("mid"), Some(out), None),
            ],
        )
    };
    // an errored chain taints its observation and must add no counts
    let errored = trace(
        4,
        vec![
            span(1, "a", json!({"q": 1}), Some(json!("mid")), None),
            span(2, "b", json!("mid"), None, Some("boom".into())),
        ],
    );
    let (_d, store) = store_with(vec![
        chain(1, json!("zebra")),
        chain(2, json!("zebra")),
        chain(3, json!("apple")),
        errored,
    ]);
    let c = contract(Scope::Region {
        from: "a".into(),
        to: "b".into(),
    });
    let region = gather_region(&store, &c).expect("gather region");
    assert_eq!(region.gathered.groups.len(), 1);
    let group = region.gathered.groups.values().next().expect("one group");
    assert_eq!(group.observations, 4);
    assert_eq!(group.errors, 1);
    assert_eq!(
        group.output_counts.get(&canonical_json(&json!("zebra"))),
        Some(&2)
    );
    assert_eq!(
        group.output_counts.get(&canonical_json(&json!("apple"))),
        Some(&1)
    );
    assert_eq!(
        group.output_counts.values().sum::<usize>(),
        3,
        "the errored chain added no witness count"
    );
    // errored group: no pick, and pick_observations counts the skip
    assert_eq!(canonical_pick(group), None);
    let (pairs, errored_skipped) = pick_observations(&region.gathered);
    assert!(pairs.is_empty());
    assert_eq!(errored_skipped, 1);
}
