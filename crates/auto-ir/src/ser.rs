//! Flatbuffers serialization of [`Graph`].
//!
//! Canonical encoding: `to_bytes` validates, then writes nodes / edges /
//! regions / effect sets in their ordered-container iteration order, so equal
//! graphs produce identical bytes and `graph → bytes → graph → bytes` is
//! byte-stable. `from_bytes` verifies the buffer (flatbuffers verifier), then
//! rejects ambiguous data (duplicate ids, duplicate edges, duplicate effect
//! entries, malformed types, unknown enum values, wrong schema version), then
//! runs full semantic validation. An invalid graph can neither be emitted nor
//! loaded.

use std::collections::BTreeSet;

use flatbuffers::{FlatBufferBuilder, UnionWIPOffset, WIPOffset};

use crate::{
    CapabilityEffect, Edge, Graph, GraphId, MemoryEffect, Node, NodeId, NodeKind, Port, Region,
    RegionId, ResourceBounds, SCHEMA_VERSION, Uncertainty, ValidationError, ValueType, fb,
};

/// A serialization / deserialization failure.
#[derive(Debug, thiserror::Error)]
pub enum IrError {
    #[error("not an Auto IR buffer (missing 'AIR0' file identifier)")]
    MissingIdentifier,
    #[error("invalid flatbuffer: {0}")]
    InvalidBuffer(#[from] flatbuffers::InvalidFlatbuffer),
    #[error("unsupported schema version {found}; this build reads exactly version {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("unknown {what} value {value} on node {node}")]
    UnknownEnum {
        what: &'static str,
        value: u8,
        node: NodeId,
    },
    #[error("malformed type on node {node}: {why}")]
    MalformedType { node: NodeId, why: &'static str },
    #[error("malformed node kind union on node {0}")]
    MalformedKind(NodeId),
    #[error("duplicate node id {0} in buffer")]
    DuplicateNode(NodeId),
    #[error("duplicate edge {0:?} in buffer")]
    DuplicateEdge(Edge),
    #[error("duplicate region id {0} in buffer")]
    DuplicateRegion(RegionId),
    #[error("duplicate capability effect on node {0}")]
    DuplicateCapability(NodeId),
    #[error("duplicate memory effect on node {0}")]
    DuplicateMemory(NodeId),
    #[error("duplicate node {node} in region {region}")]
    DuplicateRegionMember { region: RegionId, node: NodeId },
    #[error("invalid graph: {0}")]
    Validation(#[from] ValidationError),
}

/// Serialize a graph to canonical flatbuffers bytes. Validates first: an
/// invalid graph is never emitted.
pub fn to_bytes(graph: &Graph) -> Result<Vec<u8>, IrError> {
    graph.validate()?;
    let mut fbb = FlatBufferBuilder::new();

    let node_offsets: Vec<WIPOffset<fb::Node<'_>>> = graph
        .nodes
        .values()
        .map(|n| build_node(&mut fbb, n))
        .collect();
    let nodes = fbb.create_vector(&node_offsets);

    let edge_structs: Vec<fb::Edge> = graph
        .edges
        .iter()
        .map(|e| fb::Edge::new(e.from.0, e.from_port, e.to.0, e.to_port))
        .collect();
    let edges = fbb.create_vector(&edge_structs);

    let region_offsets: Vec<WIPOffset<fb::Region<'_>>> = graph
        .regions
        .values()
        .map(|r| build_region(&mut fbb, r))
        .collect();
    let regions = fbb.create_vector(&region_offsets);

    let name = fbb.create_string(&graph.name);
    let root = fb::Graph::create(
        &mut fbb,
        &fb::GraphArgs {
            schema_version: SCHEMA_VERSION,
            id_hi: (graph.id.0 >> 64) as u64,
            id_lo: graph.id.0 as u64,
            name: Some(name),
            nodes: Some(nodes),
            edges: Some(edges),
            regions: Some(regions),
        },
    );
    fb::finish_graph_buffer(&mut fbb, root);
    Ok(fbb.finished_data().to_vec())
}

/// Deserialize and fully validate a graph from bytes.
pub fn from_bytes(bytes: &[u8]) -> Result<Graph, IrError> {
    // u32 root offset + 4-byte file identifier; buffer_has_identifier asserts
    // (panics) below this length instead of returning false, so guard first.
    const MIN_BUFFER_LEN: usize = 8;
    if bytes.len() < MIN_BUFFER_LEN || !fb::graph_buffer_has_identifier(bytes) {
        return Err(IrError::MissingIdentifier);
    }
    let root = fb::root_as_graph(bytes)?;
    let found = root.schema_version();
    if found != SCHEMA_VERSION {
        return Err(IrError::UnsupportedSchemaVersion {
            found,
            supported: SCHEMA_VERSION,
        });
    }

    let id = GraphId((u128::from(root.id_hi()) << 64) | u128::from(root.id_lo()));
    let mut graph = Graph::new(id, root.name());

    for node in root.nodes().iter() {
        let node = decode_node(node)?;
        let id = node.id;
        if graph.nodes.insert(id, node).is_some() {
            return Err(IrError::DuplicateNode(id));
        }
    }
    for e in root.edges().iter() {
        let edge = Edge {
            from: NodeId(e.from_node()),
            from_port: e.from_port(),
            to: NodeId(e.to_node()),
            to_port: e.to_port(),
        };
        if !graph.edges.insert(edge) {
            return Err(IrError::DuplicateEdge(edge));
        }
    }
    for r in root.regions().iter() {
        let rid = RegionId(r.id());
        let mut members = BTreeSet::new();
        for m in r.nodes().iter() {
            if !members.insert(NodeId(m)) {
                return Err(IrError::DuplicateRegionMember {
                    region: rid,
                    node: NodeId(m),
                });
            }
        }
        let region = Region {
            id: rid,
            name: r.name().to_owned(),
            nodes: members,
        };
        if graph.regions.insert(rid, region).is_some() {
            return Err(IrError::DuplicateRegion(rid));
        }
    }

    graph.validate()?;
    Ok(graph)
}

fn build_node<'a>(fbb: &mut FlatBufferBuilder<'a>, node: &Node) -> WIPOffset<fb::Node<'a>> {
    let name = fbb.create_string(&node.name);
    let (kind_type, kind) = build_kind(fbb, &node.kind);
    let inputs = build_ports(fbb, &node.inputs);
    let outputs = build_ports(fbb, &node.outputs);
    let caps: Vec<fb::CapabilityEffect> = node.capabilities.iter().map(|&c| cap_to_fb(c)).collect();
    let capabilities = fbb.create_vector(&caps);
    let mems: Vec<fb::MemoryEffect> = node.memory.iter().map(|&m| mem_to_fb(m)).collect();
    let memory = fbb.create_vector(&mems);
    let bounds = fb::ResourceBounds::create(
        fbb,
        &fb::ResourceBoundsArgs {
            max_latency_ms: node.bounds.max_latency_ms,
            max_cost_usd_micros: node.bounds.max_cost_usd_micros,
            max_tokens: node.bounds.max_tokens,
            max_memory_bytes: node.bounds.max_memory_bytes,
        },
    );
    fb::Node::create(
        fbb,
        &fb::NodeArgs {
            id: node.id.0,
            name: Some(name),
            kind_type,
            kind: Some(kind),
            inputs: Some(inputs),
            outputs: Some(outputs),
            capabilities: Some(capabilities),
            memory: Some(memory),
            uncertainty: unc_to_fb(node.uncertainty),
            bounds: Some(bounds),
        },
    )
}

fn build_kind(
    fbb: &mut FlatBufferBuilder<'_>,
    kind: &NodeKind,
) -> (fb::NodeKind, WIPOffset<UnionWIPOffset>) {
    match kind {
        NodeKind::Input => (
            fb::NodeKind::InputK,
            fb::InputK::create(fbb, &fb::InputKArgs {}).as_union_value(),
        ),
        NodeKind::Output => (
            fb::NodeKind::OutputK,
            fb::OutputK::create(fbb, &fb::OutputKArgs {}).as_union_value(),
        ),
        NodeKind::Branch => (
            fb::NodeKind::BranchK,
            fb::BranchK::create(fbb, &fb::BranchKArgs {}).as_union_value(),
        ),
        NodeKind::ToolCall { tool } => {
            let tool = fbb.create_string(tool);
            (
                fb::NodeKind::ToolCallK,
                fb::ToolCallK::create(fbb, &fb::ToolCallKArgs { tool: Some(tool) })
                    .as_union_value(),
            )
        }
        NodeKind::ModelCall { model_class } => {
            let model_class = fbb.create_string(model_class);
            (
                fb::NodeKind::ModelCallK,
                fb::ModelCallK::create(
                    fbb,
                    &fb::ModelCallKArgs {
                        model_class: Some(model_class),
                    },
                )
                .as_union_value(),
            )
        }
        NodeKind::Transform { op } => {
            let op = fbb.create_string(op);
            (
                fb::NodeKind::TransformK,
                fb::TransformK::create(fbb, &fb::TransformKArgs { op: Some(op) }).as_union_value(),
            )
        }
    }
}

fn build_ports<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    ports: &[Port],
) -> WIPOffset<flatbuffers::Vector<'a, flatbuffers::ForwardsUOffset<fb::Port<'a>>>> {
    let offsets: Vec<WIPOffset<fb::Port<'_>>> = ports
        .iter()
        .map(|p| {
            let name = fbb.create_string(&p.name);
            let ty = build_type(fbb, &p.ty);
            fb::Port::create(
                fbb,
                &fb::PortArgs {
                    name: Some(name),
                    ty: Some(ty),
                },
            )
        })
        .collect();
    fbb.create_vector(&offsets)
}

fn build_type<'a>(fbb: &mut FlatBufferBuilder<'a>, ty: &ValueType) -> WIPOffset<fb::TypeRef<'a>> {
    let (kind, elem) = match ty {
        ValueType::Unit => (fb::TypeKind::Unit, None),
        ValueType::Bool => (fb::TypeKind::Bool, None),
        ValueType::Int => (fb::TypeKind::Int, None),
        ValueType::Float => (fb::TypeKind::Float, None),
        ValueType::Text => (fb::TypeKind::Text, None),
        ValueType::Bytes => (fb::TypeKind::Bytes, None),
        ValueType::Json => (fb::TypeKind::Json, None),
        ValueType::List(elem) => (fb::TypeKind::List, Some(build_type(fbb, elem))),
    };
    fb::TypeRef::create(fbb, &fb::TypeRefArgs { kind, elem })
}

fn build_region<'a>(fbb: &mut FlatBufferBuilder<'a>, region: &Region) -> WIPOffset<fb::Region<'a>> {
    let name = fbb.create_string(&region.name);
    let members: Vec<u64> = region.nodes.iter().map(|n| n.0).collect();
    let nodes = fbb.create_vector(&members);
    fb::Region::create(
        fbb,
        &fb::RegionArgs {
            id: region.id.0,
            name: Some(name),
            nodes: Some(nodes),
        },
    )
}

fn decode_node(n: fb::Node<'_>) -> Result<Node, IrError> {
    let id = NodeId(n.id());
    let kind = decode_kind(&n, id)?;
    let inputs = decode_ports(n.inputs(), id)?;
    let outputs = decode_ports(n.outputs(), id)?;

    let mut capabilities = BTreeSet::new();
    for c in n.capabilities().iter() {
        let effect = cap_from_fb(c).ok_or(IrError::UnknownEnum {
            what: "capability effect",
            value: c.0,
            node: id,
        })?;
        if !capabilities.insert(effect) {
            return Err(IrError::DuplicateCapability(id));
        }
    }
    let mut memory = BTreeSet::new();
    for m in n.memory().iter() {
        let effect = mem_from_fb(m).ok_or(IrError::UnknownEnum {
            what: "memory effect",
            value: m.0,
            node: id,
        })?;
        if !memory.insert(effect) {
            return Err(IrError::DuplicateMemory(id));
        }
    }
    let uncertainty = unc_from_fb(n.uncertainty()).ok_or(IrError::UnknownEnum {
        what: "uncertainty",
        value: n.uncertainty().0,
        node: id,
    })?;
    let b = n.bounds();
    let bounds = ResourceBounds {
        max_latency_ms: b.max_latency_ms(),
        max_cost_usd_micros: b.max_cost_usd_micros(),
        max_tokens: b.max_tokens(),
        max_memory_bytes: b.max_memory_bytes(),
    };

    Ok(Node {
        id,
        name: n.name().to_owned(),
        kind,
        inputs,
        outputs,
        capabilities,
        memory,
        uncertainty,
        bounds,
    })
}

fn decode_kind(n: &fb::Node<'_>, id: NodeId) -> Result<NodeKind, IrError> {
    match n.kind_type() {
        fb::NodeKind::InputK => n
            .kind_as_input_k()
            .map(|_| NodeKind::Input)
            .ok_or(IrError::MalformedKind(id)),
        fb::NodeKind::OutputK => n
            .kind_as_output_k()
            .map(|_| NodeKind::Output)
            .ok_or(IrError::MalformedKind(id)),
        fb::NodeKind::BranchK => n
            .kind_as_branch_k()
            .map(|_| NodeKind::Branch)
            .ok_or(IrError::MalformedKind(id)),
        fb::NodeKind::ToolCallK => n
            .kind_as_tool_call_k()
            .map(|t| NodeKind::ToolCall {
                tool: t.tool().to_owned(),
            })
            .ok_or(IrError::MalformedKind(id)),
        fb::NodeKind::ModelCallK => n
            .kind_as_model_call_k()
            .map(|m| NodeKind::ModelCall {
                model_class: m.model_class().to_owned(),
            })
            .ok_or(IrError::MalformedKind(id)),
        fb::NodeKind::TransformK => n
            .kind_as_transform_k()
            .map(|t| NodeKind::Transform {
                op: t.op().to_owned(),
            })
            .ok_or(IrError::MalformedKind(id)),
        other => Err(IrError::UnknownEnum {
            what: "node kind",
            value: other.0,
            node: id,
        }),
    }
}

fn decode_ports<'a>(
    v: flatbuffers::Vector<'a, flatbuffers::ForwardsUOffset<fb::Port<'a>>>,
    id: NodeId,
) -> Result<Vec<Port>, IrError> {
    let mut out = Vec::with_capacity(v.len());
    for p in v.iter() {
        out.push(Port {
            name: p.name().to_owned(),
            ty: decode_type(p.ty(), id)?,
        });
    }
    Ok(out)
}

fn decode_type(t: fb::TypeRef<'_>, node: NodeId) -> Result<ValueType, IrError> {
    match t.kind() {
        fb::TypeKind::List => {
            let elem = t.elem().ok_or(IrError::MalformedType {
                node,
                why: "list type requires elem",
            })?;
            Ok(ValueType::List(Box::new(decode_type(elem, node)?)))
        }
        other => {
            if t.elem().is_some() {
                return Err(IrError::MalformedType {
                    node,
                    why: "non-list type carries elem",
                });
            }
            match other {
                fb::TypeKind::Unit => Ok(ValueType::Unit),
                fb::TypeKind::Bool => Ok(ValueType::Bool),
                fb::TypeKind::Int => Ok(ValueType::Int),
                fb::TypeKind::Float => Ok(ValueType::Float),
                fb::TypeKind::Text => Ok(ValueType::Text),
                fb::TypeKind::Bytes => Ok(ValueType::Bytes),
                fb::TypeKind::Json => Ok(ValueType::Json),
                unknown => Err(IrError::UnknownEnum {
                    what: "type kind",
                    value: unknown.0,
                    node,
                }),
            }
        }
    }
}

fn cap_to_fb(e: CapabilityEffect) -> fb::CapabilityEffect {
    match e {
        CapabilityEffect::Net => fb::CapabilityEffect::Net,
        CapabilityEffect::Fs => fb::CapabilityEffect::Fs,
        CapabilityEffect::Exec => fb::CapabilityEffect::Exec,
        CapabilityEffect::Secrets => fb::CapabilityEffect::Secrets,
        CapabilityEffect::Payments => fb::CapabilityEffect::Payments,
    }
}

fn cap_from_fb(e: fb::CapabilityEffect) -> Option<CapabilityEffect> {
    match e {
        fb::CapabilityEffect::Net => Some(CapabilityEffect::Net),
        fb::CapabilityEffect::Fs => Some(CapabilityEffect::Fs),
        fb::CapabilityEffect::Exec => Some(CapabilityEffect::Exec),
        fb::CapabilityEffect::Secrets => Some(CapabilityEffect::Secrets),
        fb::CapabilityEffect::Payments => Some(CapabilityEffect::Payments),
        _ => None,
    }
}

fn mem_to_fb(e: MemoryEffect) -> fb::MemoryEffect {
    match e {
        MemoryEffect::Read => fb::MemoryEffect::Read,
        MemoryEffect::Write => fb::MemoryEffect::Write,
        MemoryEffect::Append => fb::MemoryEffect::Append,
    }
}

fn mem_from_fb(e: fb::MemoryEffect) -> Option<MemoryEffect> {
    match e {
        fb::MemoryEffect::Read => Some(MemoryEffect::Read),
        fb::MemoryEffect::Write => Some(MemoryEffect::Write),
        fb::MemoryEffect::Append => Some(MemoryEffect::Append),
        _ => None,
    }
}

fn unc_to_fb(u: Uncertainty) -> fb::Uncertainty {
    match u {
        Uncertainty::Deterministic => fb::Uncertainty::Deterministic,
        Uncertainty::Probabilistic => fb::Uncertainty::Probabilistic,
        Uncertainty::Generative => fb::Uncertainty::Generative,
    }
}

fn unc_from_fb(u: fb::Uncertainty) -> Option<Uncertainty> {
    match u {
        fb::Uncertainty::Deterministic => Some(Uncertainty::Deterministic),
        fb::Uncertainty::Probabilistic => Some(Uncertainty::Probabilistic),
        fb::Uncertainty::Generative => Some(Uncertainty::Generative),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::examples;

    /// Knobs for hand-building raw buffers that the canonical writer would
    /// never produce.
    struct RawOpts {
        schema_version: u32,
        caps: Vec<u8>,
        mem: Vec<u8>,
        uncertainty: u8,
        type_kind: u8,
        with_elem: bool,
        dup_node: bool,
        edges: Vec<(u64, u32, u64, u32)>,
        region_members: Option<Vec<u64>>,
    }

    impl Default for RawOpts {
        fn default() -> Self {
            Self {
                schema_version: SCHEMA_VERSION,
                caps: Vec::new(),
                mem: Vec::new(),
                uncertainty: 0,
                type_kind: 4, // Text
                with_elem: false,
                dup_node: false,
                edges: Vec::new(),
                region_members: None,
            }
        }
    }

    /// Build a raw graph buffer: one Input node (id 0) with one output port,
    /// plus whatever `RawOpts` injects.
    fn raw_graph(o: &RawOpts) -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();
        let elem = o.with_elem.then(|| {
            fb::TypeRef::create(
                &mut fbb,
                &fb::TypeRefArgs {
                    kind: fb::TypeKind::Unit,
                    elem: None,
                },
            )
        });
        let ty = fb::TypeRef::create(
            &mut fbb,
            &fb::TypeRefArgs {
                kind: fb::TypeKind(o.type_kind),
                elem,
            },
        );
        let pname = fbb.create_string("out");
        let port = fb::Port::create(
            &mut fbb,
            &fb::PortArgs {
                name: Some(pname),
                ty: Some(ty),
            },
        );
        let outputs = fbb.create_vector(&[port]);
        let inputs = fbb.create_vector::<WIPOffset<fb::Port<'_>>>(&[]);
        let caps: Vec<fb::CapabilityEffect> =
            o.caps.iter().map(|&b| fb::CapabilityEffect(b)).collect();
        let capabilities = fbb.create_vector(&caps);
        let mems: Vec<fb::MemoryEffect> = o.mem.iter().map(|&b| fb::MemoryEffect(b)).collect();
        let memory = fbb.create_vector(&mems);
        let bounds = fb::ResourceBounds::create(&mut fbb, &fb::ResourceBoundsArgs::default());
        let kind = fb::InputK::create(&mut fbb, &fb::InputKArgs {}).as_union_value();
        let name = fbb.create_string("n");
        let node = fb::Node::create(
            &mut fbb,
            &fb::NodeArgs {
                id: 0,
                name: Some(name),
                kind_type: fb::NodeKind::InputK,
                kind: Some(kind),
                inputs: Some(inputs),
                outputs: Some(outputs),
                capabilities: Some(capabilities),
                memory: Some(memory),
                uncertainty: fb::Uncertainty(o.uncertainty),
                bounds: Some(bounds),
            },
        );
        let node_list = if o.dup_node {
            vec![node, node]
        } else {
            vec![node]
        };
        let nodes = fbb.create_vector(&node_list);
        let edge_structs: Vec<fb::Edge> = o
            .edges
            .iter()
            .map(|&(f, fp, t, tp)| fb::Edge::new(f, fp, t, tp))
            .collect();
        let edges = fbb.create_vector(&edge_structs);
        let region_offsets: Vec<WIPOffset<fb::Region<'_>>> = o
            .region_members
            .iter()
            .map(|members| {
                let rname = fbb.create_string("raw-region");
                let member_vec = fbb.create_vector(members);
                fb::Region::create(
                    &mut fbb,
                    &fb::RegionArgs {
                        id: 0,
                        name: Some(rname),
                        nodes: Some(member_vec),
                    },
                )
            })
            .collect();
        let regions = fbb.create_vector(&region_offsets);
        let gname = fbb.create_string("raw");
        let graph = fb::Graph::create(
            &mut fbb,
            &fb::GraphArgs {
                schema_version: o.schema_version,
                id_hi: 0,
                id_lo: 0,
                name: Some(gname),
                nodes: Some(nodes),
                edges: Some(edges),
                regions: Some(regions),
            },
        );
        fb::finish_graph_buffer(&mut fbb, graph);
        fbb.finished_data().to_vec()
    }

    #[test]
    fn examples_roundtrip_lossless_and_byte_stable() {
        for g in [
            examples::tool_chain(),
            examples::branching(),
            examples::generative_effects(),
        ] {
            let bytes = to_bytes(&g).expect("serialize");
            let decoded = from_bytes(&bytes).expect("deserialize");
            assert_eq!(decoded, g, "round-trip must be lossless");
            let bytes2 = to_bytes(&decoded).expect("re-serialize");
            assert_eq!(bytes2, bytes, "round-trip must be byte-stable");
        }
    }

    #[test]
    fn empty_graph_roundtrips() {
        let g = Graph::new(GraphId(0), "");
        let bytes = to_bytes(&g).expect("serialize");
        assert_eq!(from_bytes(&bytes).expect("deserialize"), g);
    }

    #[test]
    fn construction_order_does_not_change_bytes() {
        let make = |node_order: [u64; 4], edge_order: [usize; 2]| {
            let mut g = Graph::new(GraphId(9), "order");
            let nodes = |i: u64| match i {
                0 => Node::new(NodeId(0), "a", NodeKind::Input)
                    .with_outputs(vec![Port::new("v", ValueType::Text)]),
                1 => Node::new(NodeId(1), "b", NodeKind::Input)
                    .with_outputs(vec![Port::new("v", ValueType::Text)]),
                2 => Node::new(NodeId(2), "x", NodeKind::Output)
                    .with_inputs(vec![Port::new("v", ValueType::Text)]),
                _ => Node::new(NodeId(3), "y", NodeKind::Output)
                    .with_inputs(vec![Port::new("v", ValueType::Text)]),
            };
            for i in node_order {
                g.insert_node(nodes(i)).unwrap();
            }
            let all_edges = [
                Edge {
                    from: NodeId(0),
                    from_port: 0,
                    to: NodeId(2),
                    to_port: 0,
                },
                Edge {
                    from: NodeId(1),
                    from_port: 0,
                    to: NodeId(3),
                    to_port: 0,
                },
            ];
            for i in edge_order {
                g.insert_edge(all_edges[i]).unwrap();
            }
            to_bytes(&g).expect("serialize")
        };
        assert_eq!(make([0, 1, 2, 3], [0, 1]), make([3, 2, 1, 0], [1, 0]));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(matches!(
            from_bytes(b"definitely not a graph"),
            Err(IrError::MissingIdentifier)
        ));
    }

    #[test]
    fn short_buffer_is_rejected() {
        let bytes = to_bytes(&examples::tool_chain()).unwrap();
        assert!(matches!(
            from_bytes(&bytes[..6]),
            Err(IrError::MissingIdentifier)
        ));
    }

    #[test]
    fn truncated_buffer_with_identifier_is_rejected() {
        let bytes = to_bytes(&examples::tool_chain()).unwrap();
        // identifier lives at bytes 4..8; keep it, drop the body
        assert!(matches!(
            from_bytes(&bytes[..12]),
            Err(IrError::InvalidBuffer(_))
        ));
    }

    #[test]
    fn future_schema_version_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            schema_version: SCHEMA_VERSION + 1,
            ..RawOpts::default()
        });
        match from_bytes(&bytes) {
            Err(IrError::UnsupportedSchemaVersion { found, supported }) => {
                assert_eq!(found, SCHEMA_VERSION + 1);
                assert_eq!(supported, SCHEMA_VERSION);
            }
            other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
        }
    }

    #[test]
    fn raw_baseline_decodes() {
        // sanity: the raw builder with defaults is a valid graph
        let bytes = raw_graph(&RawOpts::default());
        let g = from_bytes(&bytes).expect("baseline raw graph decodes");
        assert_eq!(g.nodes.len(), 1);
    }

    #[test]
    fn unknown_capability_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            caps: vec![99],
            ..RawOpts::default()
        });
        assert!(matches!(
            from_bytes(&bytes),
            Err(IrError::UnknownEnum {
                what: "capability effect",
                value: 99,
                ..
            })
        ));
    }

    #[test]
    fn duplicate_capability_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            caps: vec![0, 0],
            ..RawOpts::default()
        });
        assert!(matches!(
            from_bytes(&bytes),
            Err(IrError::DuplicateCapability(NodeId(0)))
        ));
    }

    #[test]
    fn unknown_memory_effect_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            mem: vec![77],
            ..RawOpts::default()
        });
        assert!(matches!(
            from_bytes(&bytes),
            Err(IrError::UnknownEnum {
                what: "memory effect",
                value: 77,
                ..
            })
        ));
    }

    #[test]
    fn duplicate_memory_effect_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            mem: vec![1, 1],
            ..RawOpts::default()
        });
        assert!(matches!(
            from_bytes(&bytes),
            Err(IrError::DuplicateMemory(NodeId(0)))
        ));
    }

    #[test]
    fn unknown_uncertainty_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            uncertainty: 9,
            ..RawOpts::default()
        });
        assert!(matches!(
            from_bytes(&bytes),
            Err(IrError::UnknownEnum {
                what: "uncertainty",
                value: 9,
                ..
            })
        ));
    }

    #[test]
    fn unknown_type_kind_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            type_kind: 42,
            ..RawOpts::default()
        });
        assert!(matches!(
            from_bytes(&bytes),
            Err(IrError::UnknownEnum {
                what: "type kind",
                value: 42,
                ..
            })
        ));
    }

    #[test]
    fn list_without_elem_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            type_kind: 7, // List
            with_elem: false,
            ..RawOpts::default()
        });
        assert!(matches!(
            from_bytes(&bytes),
            Err(IrError::MalformedType { .. })
        ));
    }

    #[test]
    fn scalar_with_elem_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            type_kind: 4, // Text
            with_elem: true,
            ..RawOpts::default()
        });
        assert!(matches!(
            from_bytes(&bytes),
            Err(IrError::MalformedType { .. })
        ));
    }

    #[test]
    fn duplicate_node_id_in_buffer_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            dup_node: true,
            ..RawOpts::default()
        });
        assert!(matches!(
            from_bytes(&bytes),
            Err(IrError::DuplicateNode(NodeId(0)))
        ));
    }

    #[test]
    fn duplicate_edge_in_buffer_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            edges: vec![(0, 0, 0, 0), (0, 0, 0, 0)],
            ..RawOpts::default()
        });
        assert!(matches!(from_bytes(&bytes), Err(IrError::DuplicateEdge(_))));
    }

    #[test]
    fn duplicate_region_member_is_rejected() {
        let bytes = raw_graph(&RawOpts {
            region_members: Some(vec![0, 0]),
            ..RawOpts::default()
        });
        assert!(matches!(
            from_bytes(&bytes),
            Err(IrError::DuplicateRegionMember { .. })
        ));
    }

    #[test]
    fn invalid_graph_fails_validation_on_load() {
        // structurally fine buffer, semantically broken graph: edge to nowhere
        let bytes = raw_graph(&RawOpts {
            edges: vec![(0, 0, 42, 0)],
            ..RawOpts::default()
        });
        assert!(matches!(from_bytes(&bytes), Err(IrError::Validation(_))));
    }
}
