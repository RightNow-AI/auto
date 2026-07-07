//! Property tests: arbitrary valid graphs round-trip losslessly and
//! byte-stably through the flatbuffers encoding.
//!
//! Strategy: generate free-form node "recipes", then deterministically wire
//! them into a valid typed DAG — every input port is driven by exactly one
//! existing upstream output of the same type, kind arities are honored, and
//! regions only reference real nodes. Validity-by-construction is itself
//! asserted as a property, so a generator bug cannot silently weaken the
//! round-trip property.

use std::collections::BTreeSet;

use auto_ir::{
    CapabilityEffect, Edge, Graph, GraphId, MemoryEffect, Node, NodeId, NodeKind, Port, Region,
    RegionId, ResourceBounds, Uncertainty, ValueType, from_bytes, to_bytes,
};
use proptest::prelude::*;

fn arb_type() -> impl Strategy<Value = ValueType> {
    let leaf = prop_oneof![
        Just(ValueType::Unit),
        Just(ValueType::Bool),
        Just(ValueType::Int),
        Just(ValueType::Float),
        Just(ValueType::Text),
        Just(ValueType::Bytes),
        Just(ValueType::Json),
    ];
    leaf.prop_recursive(3, 8, 1, |inner| {
        inner.prop_map(|t| ValueType::List(Box::new(t)))
    })
}

fn arb_name() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..8).prop_map(|chars| chars.into_iter().collect())
}

fn arb_uncertainty() -> impl Strategy<Value = Uncertainty> {
    prop_oneof![
        Just(Uncertainty::Deterministic),
        Just(Uncertainty::Probabilistic),
        Just(Uncertainty::Generative),
    ]
}

fn arb_caps() -> impl Strategy<Value = BTreeSet<CapabilityEffect>> {
    prop::collection::btree_set(
        prop_oneof![
            Just(CapabilityEffect::Net),
            Just(CapabilityEffect::Fs),
            Just(CapabilityEffect::Exec),
            Just(CapabilityEffect::Secrets),
            Just(CapabilityEffect::Payments),
        ],
        0..=5,
    )
}

fn arb_mem() -> impl Strategy<Value = BTreeSet<MemoryEffect>> {
    prop::collection::btree_set(
        prop_oneof![
            Just(MemoryEffect::Read),
            Just(MemoryEffect::Write),
            Just(MemoryEffect::Append),
        ],
        0..=3,
    )
}

fn arb_bounds() -> impl Strategy<Value = ResourceBounds> {
    (
        prop::option::of(any::<u64>()),
        prop::option::of(any::<u64>()),
        prop::option::of(any::<u64>()),
        prop::option::of(any::<u64>()),
    )
        .prop_map(|(lat, cost, tokens, mem)| ResourceBounds {
            max_latency_ms: lat,
            max_cost_usd_micros: cost,
            max_tokens: tokens,
            max_memory_bytes: mem,
        })
}

#[derive(Debug, Clone)]
struct NodeRecipe {
    /// 0 input, 1 output, 2 tool_call, 3 model_call, 4 transform, 5 branch
    kind_sel: u8,
    /// payload for tool/model_class/op
    label: String,
    name: String,
    n_in: usize,
    outs: Vec<(String, ValueType)>,
    in_names: Vec<String>,
    /// wiring choices, taken mod the upstream-output pool size
    srcs: Vec<u64>,
    caps: BTreeSet<CapabilityEffect>,
    mem: BTreeSet<MemoryEffect>,
    uncertainty: Uncertainty,
    bounds: ResourceBounds,
}

fn arb_recipe() -> impl Strategy<Value = NodeRecipe> {
    let shape = (
        0u8..6,
        arb_name(),
        arb_name(),
        0usize..3,
        prop::collection::vec((arb_name(), arb_type()), 0..3),
        prop::collection::vec(arb_name(), 3),
    );
    let flavor = (
        prop::collection::vec(any::<u64>(), 3),
        arb_caps(),
        arb_mem(),
        arb_uncertainty(),
        arb_bounds(),
    );
    (shape, flavor).prop_map(
        |(
            (kind_sel, label, name, n_in, outs, in_names),
            (srcs, caps, mem, uncertainty, bounds),
        )| {
            NodeRecipe {
                kind_sel,
                label,
                name,
                n_in,
                outs,
                in_names,
                srcs,
                caps,
                mem,
                uncertainty,
                bounds,
            }
        },
    )
}

fn one_out(recipe: &NodeRecipe) -> Vec<(String, ValueType)> {
    vec![
        recipe
            .outs
            .first()
            .cloned()
            .unwrap_or_else(|| ("v".to_owned(), ValueType::Text)),
    ]
}

/// Deterministically assemble a valid graph from free-form recipes.
fn build_graph(
    id: u128,
    name: String,
    recipes: Vec<NodeRecipe>,
    regions: Vec<(String, Vec<u64>)>,
) -> Graph {
    let mut g = Graph::new(GraphId(id), name);
    // every output produced so far: (node, out port, type)
    let mut pool: Vec<(NodeId, u32, ValueType)> = Vec::new();

    for (i, r) in recipes.into_iter().enumerate() {
        let node_id = NodeId(u64::try_from(i).expect("recipe count fits u64"));
        let desired_in = if pool.is_empty() { 0 } else { r.n_in };
        let (kind, n_in, out_specs) = match r.kind_sel {
            0 => (NodeKind::Input, 0, one_out(&r)),
            1 => {
                if pool.is_empty() {
                    // nothing can drive an Output yet; degrade to Input
                    (NodeKind::Input, 0, one_out(&r))
                } else {
                    (NodeKind::Output, 1, Vec::new())
                }
            }
            2 => (
                NodeKind::ToolCall {
                    tool: r.label.clone(),
                },
                desired_in,
                r.outs.clone(),
            ),
            3 => (
                NodeKind::ModelCall {
                    model_class: r.label.clone(),
                },
                desired_in,
                r.outs.clone(),
            ),
            4 => (
                NodeKind::Transform {
                    op: r.label.clone(),
                },
                desired_in,
                r.outs.clone(),
            ),
            _ => {
                if pool.is_empty() {
                    (NodeKind::Input, 0, one_out(&r))
                } else {
                    let mut outs = r.outs.clone();
                    while outs.len() < 2 {
                        outs.push((format!("branch{}", outs.len()), ValueType::Json));
                    }
                    (NodeKind::Branch, r.n_in.max(1), outs)
                }
            }
        };

        let mut inputs = Vec::with_capacity(n_in);
        for j in 0..n_in {
            let pool_len = u64::try_from(pool.len()).expect("pool fits u64");
            let pick = usize::try_from(r.srcs[j] % pool_len).expect("index fits usize");
            let (src, src_port, ty) = pool[pick].clone();
            inputs.push(Port::new(r.in_names[j].clone(), ty));
            g.insert_edge(Edge {
                from: src,
                from_port: src_port,
                to: node_id,
                to_port: u32::try_from(j).expect("port fits u32"),
            })
            .expect("edges are unique: distinct (to, to_port) per input");
        }

        let outputs: Vec<Port> = out_specs
            .into_iter()
            .map(|(port_name, ty)| Port::new(port_name, ty))
            .collect();
        for (k, port) in outputs.iter().enumerate() {
            pool.push((
                node_id,
                u32::try_from(k).expect("port fits u32"),
                port.ty.clone(),
            ));
        }

        g.insert_node(Node {
            id: node_id,
            name: r.name,
            kind,
            inputs,
            outputs,
            capabilities: r.caps,
            memory: r.mem,
            uncertainty: r.uncertainty,
            bounds: r.bounds,
        })
        .expect("node ids are fresh by construction");
    }

    let ids: Vec<NodeId> = g.nodes.keys().copied().collect();
    if !ids.is_empty() {
        for (ri, (region_name, sels)) in regions.into_iter().enumerate() {
            let id_count = u64::try_from(ids.len()).expect("node count fits u64");
            let members: BTreeSet<NodeId> = sels
                .iter()
                .map(|s| ids[usize::try_from(s % id_count).expect("index fits usize")])
                .collect();
            g.insert_region(Region {
                id: RegionId(u64::try_from(ri).expect("region count fits u64")),
                name: region_name,
                nodes: members,
            })
            .expect("region ids are fresh by construction");
        }
    }
    g
}

fn arb_graph() -> impl Strategy<Value = Graph> {
    (
        any::<u128>(),
        arb_name(),
        prop::collection::vec(arb_recipe(), 0..12),
        prop::collection::vec(
            (arb_name(), prop::collection::vec(any::<u64>(), 1..5)),
            0..3,
        ),
    )
        .prop_map(|(id, name, recipes, regions)| build_graph(id, name, recipes, regions))
}

proptest! {
    /// Generator soundness: everything we generate is a valid graph. If this
    /// fails, the round-trip property below is testing garbage.
    #[test]
    fn arbitrary_graphs_validate(g in arb_graph()) {
        prop_assert_eq!(g.validate(), Ok(()));
    }

    /// The load-bearing invariant: graph → bytes → graph → bytes is lossless
    /// and byte-stable.
    #[test]
    fn roundtrip_lossless_and_byte_stable(g in arb_graph()) {
        let bytes = to_bytes(&g).expect("serialize valid graph");
        let decoded = from_bytes(&bytes).expect("deserialize what we serialized");
        prop_assert_eq!(&decoded, &g);
        let bytes2 = to_bytes(&decoded).expect("re-serialize");
        prop_assert_eq!(bytes2, bytes);
    }
}
