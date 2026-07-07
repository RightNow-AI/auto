//! Reference graphs: hand-written IR values used by the golden tests, the
//! docs, and `auto inspect` demos.
//!
//! These are illustrations of the data model, not captured traces — trace
//! capture lands in S1.

use std::collections::BTreeSet;

use crate::{
    CapabilityEffect, Edge, Graph, GraphId, MemoryEffect, Node, NodeId, NodeKind, Port, Region,
    RegionId, ResourceBounds, Uncertainty, ValueType,
};

fn edge(from: u64, from_port: u32, to: u64, to_port: u32) -> Edge {
    Edge {
        from: NodeId(from),
        from_port,
        to: NodeId(to),
        to_port,
    }
}

fn region(id: u64, name: &str, members: &[u64]) -> Region {
    Region {
        id: RegionId(id),
        name: name.to_owned(),
        nodes: members.iter().map(|&n| NodeId(n)).collect::<BTreeSet<_>>(),
    }
}

/// Linear tool-call chain: fetch a url, parse it, store records, count out.
pub fn tool_chain() -> Graph {
    let mut g = Graph::new(GraphId(1), "tool-chain");
    let nodes = [
        Node::new(NodeId(0), "url", NodeKind::Input)
            .with_outputs(vec![Port::new("url", ValueType::Text)]),
        Node::new(
            NodeId(1),
            "fetch",
            NodeKind::ToolCall {
                tool: "http.get".into(),
            },
        )
        .with_inputs(vec![Port::new("url", ValueType::Text)])
        .with_outputs(vec![Port::new("body", ValueType::Bytes)])
        .with_capabilities([CapabilityEffect::Net])
        .with_bounds(ResourceBounds {
            max_latency_ms: Some(2000),
            ..ResourceBounds::default()
        }),
        Node::new(
            NodeId(2),
            "parse",
            NodeKind::Transform {
                op: "parse_json".into(),
            },
        )
        .with_inputs(vec![Port::new("body", ValueType::Bytes)])
        .with_outputs(vec![Port::new("records", ValueType::Json)]),
        Node::new(
            NodeId(3),
            "store",
            NodeKind::ToolCall {
                tool: "sqlite.insert".into(),
            },
        )
        .with_inputs(vec![Port::new("records", ValueType::Json)])
        .with_outputs(vec![Port::new("count", ValueType::Int)])
        .with_capabilities([CapabilityEffect::Fs]),
        Node::new(NodeId(4), "count", NodeKind::Output)
            .with_inputs(vec![Port::new("count", ValueType::Int)]),
    ];
    for node in nodes {
        g.insert_node(node).expect("unique ids by construction");
    }
    for e in [
        edge(0, 0, 1, 0),
        edge(1, 0, 2, 0),
        edge(2, 0, 3, 0),
        edge(3, 0, 4, 0),
    ] {
        g.insert_edge(e).expect("unique edges by construction");
    }
    g.insert_region(region(0, "extract-window", &[1, 2]))
        .expect("unique region id");
    debug_assert_eq!(g.validate(), Ok(()));
    g
}

/// Cache-check branch: one input, two exits (hit path and origin path).
pub fn branching() -> Graph {
    let mut g = Graph::new(GraphId(2), "branching");
    let nodes = [
        Node::new(NodeId(0), "request", NodeKind::Input)
            .with_outputs(vec![Port::new("request", ValueType::Json)]),
        Node::new(NodeId(1), "cache-check", NodeKind::Branch)
            .with_inputs(vec![Port::new("request", ValueType::Json)])
            .with_outputs(vec![
                Port::new("hit", ValueType::Json),
                Port::new("miss", ValueType::Json),
            ])
            .with_memory([MemoryEffect::Read]),
        Node::new(
            NodeId(2),
            "cached",
            NodeKind::Transform {
                op: "render_cached".into(),
            },
        )
        .with_inputs(vec![Port::new("hit", ValueType::Json)])
        .with_outputs(vec![Port::new("response", ValueType::Text)]),
        Node::new(
            NodeId(3),
            "origin",
            NodeKind::ToolCall {
                tool: "http.get".into(),
            },
        )
        .with_inputs(vec![Port::new("miss", ValueType::Json)])
        .with_outputs(vec![Port::new("response", ValueType::Text)])
        .with_capabilities([CapabilityEffect::Net])
        .with_uncertainty(Uncertainty::Probabilistic)
        .with_bounds(ResourceBounds {
            max_latency_ms: Some(5000),
            ..ResourceBounds::default()
        }),
        Node::new(NodeId(4), "cached_response", NodeKind::Output)
            .with_inputs(vec![Port::new("response", ValueType::Text)]),
        Node::new(NodeId(5), "origin_response", NodeKind::Output)
            .with_inputs(vec![Port::new("response", ValueType::Text)]),
    ];
    for node in nodes {
        g.insert_node(node).expect("unique ids by construction");
    }
    for e in [
        edge(0, 0, 1, 0),
        edge(1, 0, 2, 0),
        edge(1, 1, 3, 0),
        edge(2, 0, 4, 0),
        edge(3, 0, 5, 0),
    ] {
        g.insert_edge(e).expect("unique edges by construction");
    }
    g.insert_region(region(0, "hot-path", &[1, 2]))
        .expect("unique region id");
    debug_assert_eq!(g.validate(), Ok(()));
    g
}

/// A generative model call with declared effects and bounds, then a
/// deterministic cleanup producing a list.
pub fn generative_effects() -> Graph {
    let mut g = Graph::new(GraphId(3), "generative-effects");
    let nodes = [
        Node::new(NodeId(0), "prompt", NodeKind::Input)
            .with_outputs(vec![Port::new("prompt", ValueType::Text)]),
        Node::new(
            NodeId(1),
            "draft",
            NodeKind::ModelCall {
                model_class: "frontier".into(),
            },
        )
        .with_inputs(vec![Port::new("prompt", ValueType::Text)])
        .with_outputs(vec![Port::new("draft", ValueType::Text)])
        .with_capabilities([CapabilityEffect::Net, CapabilityEffect::Secrets])
        .with_memory([MemoryEffect::Read, MemoryEffect::Append])
        .with_uncertainty(Uncertainty::Generative)
        .with_bounds(ResourceBounds {
            max_latency_ms: Some(30_000),
            max_cost_usd_micros: Some(500_000),
            max_tokens: Some(4096),
            max_memory_bytes: None,
        }),
        Node::new(
            NodeId(2),
            "split",
            NodeKind::Transform {
                op: "split_paragraphs".into(),
            },
        )
        .with_inputs(vec![Port::new("draft", ValueType::Text)])
        .with_outputs(vec![Port::new(
            "paragraphs",
            ValueType::List(Box::new(ValueType::Text)),
        )]),
        Node::new(NodeId(3), "paragraphs", NodeKind::Output).with_inputs(vec![Port::new(
            "paragraphs",
            ValueType::List(Box::new(ValueType::Text)),
        )]),
    ];
    for node in nodes {
        g.insert_node(node).expect("unique ids by construction");
    }
    for e in [edge(0, 0, 1, 0), edge(1, 0, 2, 0), edge(2, 0, 3, 0)] {
        g.insert_edge(e).expect("unique edges by construction");
    }
    g.insert_region(region(0, "generative-core", &[1]))
        .expect("unique region id");
    debug_assert_eq!(g.validate(), Ok(()));
    g
}
