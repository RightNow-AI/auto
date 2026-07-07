use std::fmt::{self, Write};

use crate::{Graph, Node, NodeKind, Port, ResourceBounds, SCHEMA_VERSION};

/// Deterministic, human-readable rendering of a graph — what `auto inspect`
/// prints and what the golden `.txt` files pin.
///
/// Not a stable machine format: parse the flatbuffers, not this.
pub fn render(graph: &Graph) -> String {
    let mut out = String::new();
    render_into(graph, &mut out).expect("fmt::Write to String is infallible");
    out
}

fn render_into(g: &Graph, out: &mut String) -> fmt::Result {
    writeln!(
        out,
        "graph \"{}\" id={} schema_version={SCHEMA_VERSION}",
        g.name, g.id
    )?;
    writeln!(
        out,
        "nodes={} edges={} regions={}",
        g.nodes.len(),
        g.edges.len(),
        g.regions.len()
    )?;
    writeln!(out)?;

    writeln!(out, "nodes:")?;
    if g.nodes.is_empty() {
        writeln!(out, "  (none)")?;
    }
    for node in g.nodes.values() {
        render_node(node, out)?;
    }

    writeln!(out, "edges:")?;
    if g.edges.is_empty() {
        writeln!(out, "  (none)")?;
    }
    for e in &g.edges {
        writeln!(
            out,
            "  {}[{}] -> {}[{}]",
            e.from, e.from_port, e.to, e.to_port
        )?;
    }

    writeln!(out, "regions:")?;
    if g.regions.is_empty() {
        writeln!(out, "  (none)")?;
    }
    for r in g.regions.values() {
        let members: Vec<String> = r.nodes.iter().map(ToString::to_string).collect();
        writeln!(
            out,
            "  {} \"{}\" nodes={{{}}}",
            r.id,
            r.name,
            members.join(",")
        )?;
    }
    Ok(())
}

fn render_node(n: &Node, out: &mut String) -> fmt::Result {
    let kind = match &n.kind {
        NodeKind::ToolCall { tool } => format!("tool_call({tool})"),
        NodeKind::ModelCall { model_class } => format!("model_call({model_class})"),
        NodeKind::Transform { op } => format!("transform({op})"),
        other => other.label().to_owned(),
    };
    write!(
        out,
        "  {} {kind} \"{}\" : ({}) -> ({}) [{}]",
        n.id,
        n.name,
        ports(&n.inputs),
        ports(&n.outputs),
        n.uncertainty
    )?;
    if !n.capabilities.is_empty() {
        let items: Vec<String> = n.capabilities.iter().map(ToString::to_string).collect();
        write!(out, " caps={{{}}}", items.join(","))?;
    }
    if !n.memory.is_empty() {
        let items: Vec<String> = n.memory.iter().map(ToString::to_string).collect();
        write!(out, " mem={{{}}}", items.join(","))?;
    }
    if let Some(bounds) = bounds_summary(&n.bounds) {
        write!(out, " bounds={{{bounds}}}")?;
    }
    writeln!(out)
}

fn ports(ports: &[Port]) -> String {
    ports
        .iter()
        .map(|p| {
            if p.name.is_empty() {
                p.ty.to_string()
            } else {
                format!("{}:{}", p.name, p.ty)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn bounds_summary(b: &ResourceBounds) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(v) = b.max_latency_ms {
        parts.push(format!("max_latency_ms={v}"));
    }
    if let Some(v) = b.max_cost_usd_micros {
        parts.push(format!("max_cost_usd_micros={v}"));
    }
    if let Some(v) = b.max_tokens {
        parts.push(format!("max_tokens={v}"));
    }
    if let Some(v) = b.max_memory_bytes {
        parts.push(format!("max_memory_bytes={v}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}
