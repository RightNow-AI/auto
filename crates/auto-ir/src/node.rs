use std::collections::BTreeSet;

use crate::{CapabilityEffect, MemoryEffect, NodeId, Uncertainty, ValueType};

/// A named, typed port on a node. Edges attach to ports by index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    /// display name; empty is allowed and means "unnamed"
    pub name: String,
    pub ty: ValueType,
}

impl Port {
    pub fn new(name: impl Into<String>, ty: ValueType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}

/// What a node does. Arity rules per kind are enforced by
/// [`crate::Graph::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// graph entry — exactly 0 inputs, 1 output
    Input,
    /// graph exit — exactly 1 input, 0 outputs
    Output,
    /// invoke an external tool named by `tool`
    ToolCall { tool: String },
    /// invoke a model; `model_class` is a routing hint ("frontier",
    /// "distilled-0.5b", …) — never a capability or parity claim
    ModelCall { model_class: String },
    /// computation described by `op`, with no effects beyond its declared sets
    Transform { op: String },
    /// decision point — ≥1 input, ≥2 outputs; exactly one out-branch carries
    /// a value at runtime
    Branch,
}

impl NodeKind {
    /// Stable lowercase label used in rendering and error messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::ToolCall { .. } => "tool_call",
            Self::ModelCall { .. } => "model_call",
            Self::Transform { .. } => "transform",
            Self::Branch => "branch",
        }
    }
}

/// Declared per-node resource ceilings.
///
/// `None` means "not declared" — it is never a measured number and never a
/// fabricated one (CLAUDE.md: honesty is load-bearing). Measured bounds live
/// in the artifact manifest from S3 on, not here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceBounds {
    pub max_latency_ms: Option<u64>,
    /// micro-usd: 1_000_000 == $1
    pub max_cost_usd_micros: Option<u64>,
    pub max_tokens: Option<u64>,
    pub max_memory_bytes: Option<u64>,
}

/// One operation in the task graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: NodeId,
    /// display name; empty allowed
    pub name: String,
    pub kind: NodeKind,
    pub inputs: Vec<Port>,
    pub outputs: Vec<Port>,
    /// capabilities this node may exercise; empty = fully confined
    pub capabilities: BTreeSet<CapabilityEffect>,
    /// effects against the task memory store; empty = memory-silent
    pub memory: BTreeSet<MemoryEffect>,
    pub uncertainty: Uncertainty,
    pub bounds: ResourceBounds,
}

impl Node {
    /// A node with no ports, no effects, deterministic, unbounded. Add the
    /// rest with the `with_*` builders.
    pub fn new(id: NodeId, name: impl Into<String>, kind: NodeKind) -> Self {
        Self {
            id,
            name: name.into(),
            kind,
            inputs: Vec::new(),
            outputs: Vec::new(),
            capabilities: BTreeSet::new(),
            memory: BTreeSet::new(),
            uncertainty: Uncertainty::Deterministic,
            bounds: ResourceBounds::default(),
        }
    }

    #[must_use]
    pub fn with_inputs(mut self, inputs: Vec<Port>) -> Self {
        self.inputs = inputs;
        self
    }

    #[must_use]
    pub fn with_outputs(mut self, outputs: Vec<Port>) -> Self {
        self.outputs = outputs;
        self
    }

    #[must_use]
    pub fn with_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = CapabilityEffect>,
    ) -> Self {
        self.capabilities = capabilities.into_iter().collect();
        self
    }

    #[must_use]
    pub fn with_memory(mut self, memory: impl IntoIterator<Item = MemoryEffect>) -> Self {
        self.memory = memory.into_iter().collect();
        self
    }

    #[must_use]
    pub fn with_uncertainty(mut self, uncertainty: Uncertainty) -> Self {
        self.uncertainty = uncertainty;
        self
    }

    #[must_use]
    pub fn with_bounds(mut self, bounds: ResourceBounds) -> Self {
        self.bounds = bounds;
        self
    }
}
