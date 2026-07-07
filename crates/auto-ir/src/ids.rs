use std::fmt;

/// Stable identity of a node within one graph.
///
/// Ids are assigned by the producer, never reused within a graph, never
/// renumbered by any pass, and preserved bit-exactly by serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

/// Stable identity of a region within one graph. Same stability rules as
/// [`NodeId`]; node and region ids live in separate namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionId(pub u64);

/// Stable identity of a graph. 128 bits so producers can mint globally unique
/// ids (uuid-shaped) without coordination. The IR never mints ids itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphId(pub u128);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "n{}", self.0)
    }
}

impl fmt::Display for RegionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

impl fmt::Display for GraphId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}
