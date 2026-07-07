use std::fmt;

/// Type of a value flowing along an edge.
///
/// v0 is deliberately small: scalars, an opaque-structure escape hatch
/// ([`ValueType::Json`]), and homogeneous lists. Records, unions, and schema'd
/// structures are open questions (spec/adr/open-questions.md).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueType {
    /// no payload; pure sequencing
    Unit,
    Bool,
    /// 64-bit signed integer
    Int,
    /// IEEE-754 binary64
    Float,
    /// utf-8 text
    Text,
    /// opaque byte string
    Bytes,
    /// structured value with no schema commitment (escape hatch)
    Json,
    /// homogeneous list of the element type
    List(Box<ValueType>),
}

impl fmt::Display for ValueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unit => f.write_str("unit"),
            Self::Bool => f.write_str("bool"),
            Self::Int => f.write_str("int"),
            Self::Float => f.write_str("float"),
            Self::Text => f.write_str("text"),
            Self::Bytes => f.write_str("bytes"),
            Self::Json => f.write_str("json"),
            Self::List(elem) => write!(f, "list<{elem}>"),
        }
    }
}
