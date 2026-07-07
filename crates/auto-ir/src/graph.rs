use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::{GraphId, Node, NodeId, NodeKind, RegionId, ValueType};

/// Wire-format schema version written by this build and required — exactly —
/// on read. Bump policy: spec/ir.md "versioning".
pub const SCHEMA_VERSION: u32 = 0;

/// Dataflow edge `(from, from_port) → (to, to_port)`.
///
/// Port indices are positions into the source node's `outputs` and the
/// destination node's `inputs`. Edges have no identity beyond their four
/// fields; the derived ordering is the canonical serialization order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Edge {
    pub from: NodeId,
    pub from_port: u32,
    pub to: NodeId,
    pub to_port: u32,
}

/// A named group of nodes — the unit of extraction for later passes.
/// Flat (no nesting) in v0; regions may overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub id: RegionId,
    pub name: String,
    pub nodes: BTreeSet<NodeId>,
}

/// Port direction tag used in validation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDir {
    In,
    Out,
}

impl fmt::Display for PortDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::In => "input",
            Self::Out => "output",
        })
    }
}

/// A structural invariant violation. `Graph::validate` reports the first one
/// found, in a deterministic order.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("duplicate node id {0}")]
    DuplicateNode(NodeId),
    #[error("duplicate edge {0:?}")]
    DuplicateEdge(Edge),
    #[error("duplicate region id {0}")]
    DuplicateRegion(RegionId),
    #[error("node with id {actual} stored under map key {key}")]
    NodeKeyMismatch { key: NodeId, actual: NodeId },
    #[error("region with id {actual} stored under map key {key}")]
    RegionKeyMismatch { key: RegionId, actual: RegionId },
    #[error("node {node}: {count} {dir} ports exceeds u32 port addressing")]
    TooManyPorts {
        node: NodeId,
        dir: PortDir,
        count: usize,
    },
    #[error("node {node}: kind `{kind}` does not allow {inputs} input / {outputs} output ports")]
    KindArity {
        node: NodeId,
        kind: &'static str,
        inputs: usize,
        outputs: usize,
    },
    #[error("edge {edge:?} references unknown node {node}")]
    EdgeUnknownNode { edge: Edge, node: NodeId },
    #[error("edge {edge:?}: {dir} port {port} out of range for node {node} ({available} ports)")]
    EdgePortOutOfRange {
        edge: Edge,
        node: NodeId,
        dir: PortDir,
        port: u32,
        available: usize,
    },
    #[error("edge {edge:?}: type mismatch — source produces {from}, destination expects {to}")]
    EdgeTypeMismatch {
        edge: Edge,
        from: ValueType,
        to: ValueType,
    },
    #[error("input port {port} of node {node} is driven by {count} edges; exactly one required")]
    InputPortFanIn {
        node: NodeId,
        port: u32,
        count: usize,
    },
    #[error("input port {port} of node {node} is not driven")]
    InputPortUndriven { node: NodeId, port: u32 },
    #[error("graph has a cycle through node {0}")]
    Cycle(NodeId),
    #[error("region {region} references unknown node {node}")]
    RegionUnknownNode { region: RegionId, node: NodeId },
    #[error("region {0} is empty")]
    EmptyRegion(RegionId),
}

/// The typed task graph.
///
/// Containers are ordered (`BTreeMap`/`BTreeSet`): iteration order is the
/// canonical serialization order, so two equal graphs serialize to identical
/// bytes regardless of construction order.
///
/// Fields are public; [`Graph::validate`] checks every invariant and is run
/// by both `to_bytes` and `from_bytes`, so an invalid graph can neither be
/// emitted nor loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Graph {
    pub id: GraphId,
    /// display name; empty allowed
    pub name: String,
    /// keyed by `Node::id` (key == embedded id is a validated invariant)
    pub nodes: BTreeMap<NodeId, Node>,
    pub edges: BTreeSet<Edge>,
    /// keyed by `Region::id` (key == embedded id is a validated invariant)
    pub regions: BTreeMap<RegionId, Region>,
}

impl Graph {
    pub fn new(id: GraphId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            nodes: BTreeMap::new(),
            edges: BTreeSet::new(),
            regions: BTreeMap::new(),
        }
    }

    /// Insert a node under its own id. Errors on duplicate id.
    pub fn insert_node(&mut self, node: Node) -> Result<(), ValidationError> {
        if self.nodes.contains_key(&node.id) {
            return Err(ValidationError::DuplicateNode(node.id));
        }
        self.nodes.insert(node.id, node);
        Ok(())
    }

    /// Insert an edge. Errors if the identical edge is already present.
    pub fn insert_edge(&mut self, edge: Edge) -> Result<(), ValidationError> {
        if !self.edges.insert(edge) {
            return Err(ValidationError::DuplicateEdge(edge));
        }
        Ok(())
    }

    /// Insert a region under its own id. Errors on duplicate id.
    pub fn insert_region(&mut self, region: Region) -> Result<(), ValidationError> {
        if self.regions.contains_key(&region.id) {
            return Err(ValidationError::DuplicateRegion(region.id));
        }
        self.regions.insert(region.id, region);
        Ok(())
    }

    /// Check every structural invariant (spec/ir.md "invariants"): key/id
    /// agreement, port addressing, kind arity, edge endpoints and types,
    /// single-assignment input ports, acyclicity, region membership.
    pub fn validate(&self) -> Result<(), ValidationError> {
        // map keys match embedded ids
        for (key, node) in &self.nodes {
            if *key != node.id {
                return Err(ValidationError::NodeKeyMismatch {
                    key: *key,
                    actual: node.id,
                });
            }
        }
        for (key, region) in &self.regions {
            if *key != region.id {
                return Err(ValidationError::RegionKeyMismatch {
                    key: *key,
                    actual: region.id,
                });
            }
        }

        // port addressing and kind arity
        for node in self.nodes.values() {
            if u32::try_from(node.inputs.len()).is_err() {
                return Err(ValidationError::TooManyPorts {
                    node: node.id,
                    dir: PortDir::In,
                    count: node.inputs.len(),
                });
            }
            if u32::try_from(node.outputs.len()).is_err() {
                return Err(ValidationError::TooManyPorts {
                    node: node.id,
                    dir: PortDir::Out,
                    count: node.outputs.len(),
                });
            }
            let (n_in, n_out) = (node.inputs.len(), node.outputs.len());
            let arity_ok = match node.kind {
                NodeKind::Input => n_in == 0 && n_out == 1,
                NodeKind::Output => n_in == 1 && n_out == 0,
                NodeKind::Branch => n_in >= 1 && n_out >= 2,
                NodeKind::ToolCall { .. }
                | NodeKind::ModelCall { .. }
                | NodeKind::Transform { .. } => true,
            };
            if !arity_ok {
                return Err(ValidationError::KindArity {
                    node: node.id,
                    kind: node.kind.label(),
                    inputs: n_in,
                    outputs: n_out,
                });
            }
        }

        // edge endpoints, port ranges, port types
        for edge in &self.edges {
            let from = self
                .nodes
                .get(&edge.from)
                .ok_or(ValidationError::EdgeUnknownNode {
                    edge: *edge,
                    node: edge.from,
                })?;
            let to = self
                .nodes
                .get(&edge.to)
                .ok_or(ValidationError::EdgeUnknownNode {
                    edge: *edge,
                    node: edge.to,
                })?;
            let src = from.outputs.get(edge.from_port as usize).ok_or(
                ValidationError::EdgePortOutOfRange {
                    edge: *edge,
                    node: edge.from,
                    dir: PortDir::Out,
                    port: edge.from_port,
                    available: from.outputs.len(),
                },
            )?;
            let dst = to.inputs.get(edge.to_port as usize).ok_or(
                ValidationError::EdgePortOutOfRange {
                    edge: *edge,
                    node: edge.to,
                    dir: PortDir::In,
                    port: edge.to_port,
                    available: to.inputs.len(),
                },
            )?;
            if src.ty != dst.ty {
                return Err(ValidationError::EdgeTypeMismatch {
                    edge: *edge,
                    from: src.ty.clone(),
                    to: dst.ty.clone(),
                });
            }
        }

        // single assignment: every input port driven exactly once
        let mut fan_in: BTreeMap<(NodeId, u32), usize> = BTreeMap::new();
        for edge in &self.edges {
            *fan_in.entry((edge.to, edge.to_port)).or_default() += 1;
        }
        for ((node, port), count) in &fan_in {
            if *count > 1 {
                return Err(ValidationError::InputPortFanIn {
                    node: *node,
                    port: *port,
                    count: *count,
                });
            }
        }
        for node in self.nodes.values() {
            for idx in 0..node.inputs.len() {
                let port = u32::try_from(idx).expect("port count checked above");
                if !fan_in.contains_key(&(node.id, port)) {
                    return Err(ValidationError::InputPortUndriven {
                        node: node.id,
                        port,
                    });
                }
            }
        }

        // acyclicity (Kahn); all edge endpoints exist by now
        let mut indegree: BTreeMap<NodeId, usize> = self.nodes.keys().map(|&id| (id, 0)).collect();
        let mut successors: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for edge in &self.edges {
            *indegree.get_mut(&edge.to).expect("endpoint checked") += 1;
            successors.entry(edge.from).or_default().push(edge.to);
        }
        let mut ready: VecDeque<NodeId> = indegree
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(&id, _)| id)
            .collect();
        let mut visited = 0usize;
        while let Some(id) = ready.pop_front() {
            visited += 1;
            if let Some(next) = successors.get(&id) {
                for succ in next {
                    let d = indegree.get_mut(succ).expect("endpoint checked");
                    *d -= 1;
                    if *d == 0 {
                        ready.push_back(*succ);
                    }
                }
            }
        }
        if visited < self.nodes.len() {
            let stuck = indegree
                .iter()
                .find(|&(_, &d)| d > 0)
                .map(|(&id, _)| id)
                .expect("a cycle leaves positive indegree");
            return Err(ValidationError::Cycle(stuck));
        }

        // regions: non-empty, members exist
        for region in self.regions.values() {
            if region.nodes.is_empty() {
                return Err(ValidationError::EmptyRegion(region.id));
            }
            for member in &region.nodes {
                if !self.nodes.contains_key(member) {
                    return Err(ValidationError::RegionUnknownNode {
                        region: region.id,
                        node: *member,
                    });
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Port, examples};

    fn text_input(id: u64) -> Node {
        Node::new(NodeId(id), "in", NodeKind::Input)
            .with_outputs(vec![Port::new("value", ValueType::Text)])
    }

    fn output_of(id: u64, ty: ValueType) -> Node {
        Node::new(NodeId(id), "out", NodeKind::Output).with_inputs(vec![Port::new("value", ty)])
    }

    fn edge(from: u64, from_port: u32, to: u64, to_port: u32) -> Edge {
        Edge {
            from: NodeId(from),
            from_port,
            to: NodeId(to),
            to_port,
        }
    }

    #[test]
    fn examples_validate() {
        for g in [
            examples::tool_chain(),
            examples::branching(),
            examples::generative_effects(),
        ] {
            assert_eq!(g.validate(), Ok(()));
        }
    }

    #[test]
    fn empty_graph_is_valid() {
        assert_eq!(Graph::new(GraphId(0), "").validate(), Ok(()));
    }

    #[test]
    fn duplicate_node_id_rejected_on_insert() {
        let mut g = Graph::new(GraphId(1), "g");
        g.insert_node(text_input(0)).unwrap();
        assert_eq!(
            g.insert_node(text_input(0)),
            Err(ValidationError::DuplicateNode(NodeId(0)))
        );
    }

    #[test]
    fn node_key_mismatch_detected() {
        let mut g = Graph::new(GraphId(1), "g");
        g.nodes.insert(NodeId(7), text_input(0));
        assert!(matches!(
            g.validate(),
            Err(ValidationError::NodeKeyMismatch { .. })
        ));
    }

    #[test]
    fn edge_type_mismatch_rejected() {
        let mut g = Graph::new(GraphId(1), "g");
        g.insert_node(text_input(0)).unwrap();
        g.insert_node(output_of(1, ValueType::Int)).unwrap();
        g.insert_edge(edge(0, 0, 1, 0)).unwrap();
        assert!(matches!(
            g.validate(),
            Err(ValidationError::EdgeTypeMismatch { .. })
        ));
    }

    #[test]
    fn undriven_input_rejected() {
        let mut g = Graph::new(GraphId(1), "g");
        g.insert_node(output_of(0, ValueType::Text)).unwrap();
        assert_eq!(
            g.validate(),
            Err(ValidationError::InputPortUndriven {
                node: NodeId(0),
                port: 0
            })
        );
    }

    #[test]
    fn fan_in_rejected() {
        let mut g = Graph::new(GraphId(1), "g");
        g.insert_node(text_input(0)).unwrap();
        g.insert_node(text_input(1)).unwrap();
        g.insert_node(output_of(2, ValueType::Text)).unwrap();
        g.insert_edge(edge(0, 0, 2, 0)).unwrap();
        g.insert_edge(edge(1, 0, 2, 0)).unwrap();
        assert_eq!(
            g.validate(),
            Err(ValidationError::InputPortFanIn {
                node: NodeId(2),
                port: 0,
                count: 2
            })
        );
    }

    #[test]
    fn port_out_of_range_rejected() {
        let mut g = Graph::new(GraphId(1), "g");
        g.insert_node(text_input(0)).unwrap();
        g.insert_node(output_of(1, ValueType::Text)).unwrap();
        g.insert_edge(edge(0, 3, 1, 0)).unwrap();
        assert!(matches!(
            g.validate(),
            Err(ValidationError::EdgePortOutOfRange { .. })
        ));
    }

    #[test]
    fn unknown_edge_endpoint_rejected() {
        let mut g = Graph::new(GraphId(1), "g");
        g.insert_node(text_input(0)).unwrap();
        g.insert_edge(edge(0, 0, 9, 0)).unwrap();
        assert!(matches!(
            g.validate(),
            Err(ValidationError::EdgeUnknownNode { .. })
        ));
    }

    #[test]
    fn cycle_rejected() {
        let mut g = Graph::new(GraphId(1), "g");
        for id in [0u64, 1] {
            g.insert_node(
                Node::new(NodeId(id), "t", NodeKind::Transform { op: "id".into() })
                    .with_inputs(vec![Port::new("in", ValueType::Text)])
                    .with_outputs(vec![Port::new("out", ValueType::Text)]),
            )
            .unwrap();
        }
        g.insert_edge(edge(0, 0, 1, 0)).unwrap();
        g.insert_edge(edge(1, 0, 0, 0)).unwrap();
        assert!(matches!(g.validate(), Err(ValidationError::Cycle(_))));
    }

    #[test]
    fn branch_arity_enforced() {
        let mut g = Graph::new(GraphId(1), "g");
        g.insert_node(text_input(0)).unwrap();
        g.insert_node(
            Node::new(NodeId(1), "b", NodeKind::Branch)
                .with_inputs(vec![Port::new("in", ValueType::Text)])
                .with_outputs(vec![Port::new("only", ValueType::Text)]),
        )
        .unwrap();
        g.insert_edge(edge(0, 0, 1, 0)).unwrap();
        assert!(matches!(
            g.validate(),
            Err(ValidationError::KindArity { kind: "branch", .. })
        ));
    }

    #[test]
    fn input_arity_enforced() {
        let mut g = Graph::new(GraphId(1), "g");
        g.insert_node(
            Node::new(NodeId(0), "bad", NodeKind::Input)
                .with_inputs(vec![Port::new("in", ValueType::Text)])
                .with_outputs(vec![Port::new("out", ValueType::Text)]),
        )
        .unwrap();
        assert!(matches!(
            g.validate(),
            Err(ValidationError::KindArity { kind: "input", .. })
        ));
    }

    #[test]
    fn empty_region_rejected() {
        let mut g = Graph::new(GraphId(1), "g");
        g.insert_region(Region {
            id: RegionId(0),
            name: "empty".into(),
            nodes: BTreeSet::new(),
        })
        .unwrap();
        assert_eq!(g.validate(), Err(ValidationError::EmptyRegion(RegionId(0))));
    }

    #[test]
    fn region_unknown_member_rejected() {
        let mut g = Graph::new(GraphId(1), "g");
        g.insert_region(Region {
            id: RegionId(0),
            name: "ghost".into(),
            nodes: [NodeId(42)].into(),
        })
        .unwrap();
        assert!(matches!(
            g.validate(),
            Err(ValidationError::RegionUnknownNode { .. })
        ));
    }
}
