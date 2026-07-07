//! Contract data model — the type system of compiled cognition, v0.
//!
//! A contract declares, for one task or one operation signature: the typed
//! interface, normative examples, a closed set of machine-checkable
//! properties, declared resource budgets, and bulk eval cases. Format spec:
//! `spec/contract.md`; parsing lives in `parse.rs`; checking in `conform.rs`
//! and `properties.rs`; the harness in `harness.rs`.

use std::fmt;

use auto_ir::ValueType;
use serde_json::Value;

/// Wire-format version of the contract file. This build reads exactly 0.
pub const CONTRACT_VERSION: u32 = 0;

/// Content-addressed contract identity: lowercase-hex sha-256 of the
/// contract's canonical JSON form (see `Contract::canonical_json`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContractId(pub String);

impl fmt::Display for ContractId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the contract binds to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Whole-task input/output. Declarable in v0 but not verifiable against
    /// traces yet — traces carry no task-level I/O (open-questions.md).
    Task,
    /// One operation signature: every recorded span with this kind + name.
    /// `kind` is one of the effectful span kinds (spec/trace.md §2).
    Span { kind: String, name: String },
    /// A recorded CHAIN of spans, from the span named `from` through the
    /// span named `to` (inclusive, by seq order, names unique within the
    /// window). The region's interface is (from.input) -> (to.output); the
    /// chain structure must be identical across every recorded trace, and
    /// v0 regions must be pure — a tool_call or memory_op inside the chain
    /// refuses compilation (spec/synthesis.md §8, ADR-0015).
    Region { from: String, to: String },
}

/// Typed interface of the subject under contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub input: ValueType,
    pub output: ValueType,
}

/// How an example's expected output is compared. `Exact` is the default
/// posture and the only mode that needs no judge: canonical-json equality,
/// nothing else. `Judged` asks an LLM judge whether the subject's output
/// and the expected output are **semantically equivalent** for the
/// contracted task (ADR-0019) — exactly-equal outputs still pass free,
/// without consulting the judge, and a judged example with no judge
/// available is unchecked, never passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    Exact,
    Judged,
}

/// A normative example: this input must produce this output.
#[derive(Debug, Clone, PartialEq)]
pub struct Example {
    pub name: String,
    pub input: Value,
    pub output: Value,
    pub match_mode: MatchMode,
}

/// Where a property looks. v0: outputs only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Output,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Output => "output",
        })
    }
}

/// v0 property language: a small closed set of total, machine-checkable
/// predicates. A property applied to a value of the wrong shape FAILS (it
/// does not skip). Arbitrary predicates arrive with sandboxed execution
/// (S4+), never as unsandboxed code in a contract file.
#[derive(Debug, Clone, PartialEq)]
pub enum Property {
    /// length bounds: chars of text, elements of a list, chars of a
    /// bytes-string. Inclusive. None = unbounded on that side.
    LenRange {
        target: Target,
        min: Option<u64>,
        max: Option<u64>,
    },
    /// rust `regex` search semantics (anchor explicitly for full match);
    /// applies to text values only
    Regex { target: Target, pattern: String },
    /// numeric bounds, inclusive; applies to int/float values (compared as
    /// f64 — integers beyond 2^53 lose precision here; documented)
    NumRange {
        target: Target,
        min: Option<f64>,
        max: Option<f64>,
    },
    /// value is a json object containing all listed keys
    JsonHasKeys { target: Target, keys: Vec<String> },
    /// value equals (canonical-json equality) one of the listed values
    OneOf { target: Target, values: Vec<Value> },
}

/// Declared resource ceilings. `None` = not declared. A declared budget the
/// harness cannot measure makes the verdict Inconclusive, never Pass:
/// a Pass means every normative claim was actually checked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Budgets {
    /// p95 of observed/measured latencies must be <= this
    pub max_latency_ms_p95: Option<u64>,
    /// micro-usd; p95 of the reserved `cost_usd_micros` span attr
    /// (spec/trace.md §3) — measurable only when every matching recorded
    /// span carries it, and never for live subjects
    pub max_cost_usd_micros: Option<u64>,
    /// p95 of the reserved `tokens` span attr — same all-or-Inconclusive
    /// rule as cost
    pub max_tokens: Option<u64>,
}

/// How the differential gate decides whether a replayed subject output
/// matches its group's recorded reference. `Exact` is the default and the
/// only mode that needs no judge: canonical-json byte equality, nothing
/// else — v0 behavior unchanged. `Judged` (ADR-0021) lets the ADR-0019
/// judge arbitrate **byte-divergent groups only**: a group whose subject
/// output already equals its reference byte-wise passes free, without
/// consulting the judge (the wave-8 short-circuit principle); a judged-
/// equivalent group counts as matched against the declared ADR-0018
/// agreement threshold, which remains the sole acceptance authority. A
/// judged differential with no judge supplied is unchecked, never passed —
/// it never silently falls back to exact counting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DifferentialMatch {
    #[default]
    Exact,
    Judged,
}

/// Statistical acceptance: declared relaxations of reproduction claims
/// (ADR-0018, ADR-0021). The default (`None` / `Exact`) is **exact** —
/// every replayed input must reproduce its recorded output, the v0
/// behavior unchanged. Acceptance is part of contract identity: two
/// contracts differing only here make different normative claims and get
/// different ids (spec/contract.md §8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Acceptance {
    /// Minimum differential agreement rate in integer thousandths, `1..=1000`
    /// (1000 = declared-exact; the integer-milli convention of ADR-0014 —
    /// no floats in normative wire). `None` = exact, the v0 behavior
    /// unchanged. `Some` relaxes ONLY the differential reproduction claim
    /// (compiled subject vs recorded reference, ADR-0018): the gate accepts
    /// when `matched * 1000 >= milli * eligible`, integer math, and the
    /// manifest records the measured rate. Examples, properties, budgets,
    /// and interface conformance stay exact.
    pub differential_min_agreement_milli: Option<u32>,
    /// How a replayed output is compared to its group reference (ADR-0021).
    /// `Judged` consults the judge ONLY on byte-divergent groups — byte-
    /// equal groups pass free — and requires a declared
    /// `differential_min_agreement_milli`: the threshold still decides;
    /// the judge only decides what counts as matched.
    pub differential_match: DifferentialMatch,
}

/// One bulk eval case: an input, with an optional exact-match expected
/// output. Loaded from JSONL files referenced by the contract.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalCase {
    pub input: Value,
    pub expected: Option<Value>,
}

/// A parsed contract. Construction goes through `parse::load` /
/// `parse::from_toml_str`, which enforce structural validity; semantic
/// checking of subjects happens in `harness`.
#[derive(Debug, Clone, PartialEq)]
pub struct Contract {
    /// task label this contract belongs to (matches trace task labels)
    pub task: String,
    pub scope: Scope,
    pub interface: Interface,
    pub examples: Vec<Example>,
    pub properties: Vec<Property>,
    pub budgets: Budgets,
    /// statistical acceptance; the default is exact — `None` means every
    /// replayed input must reproduce its recorded output (ADR-0018)
    pub acceptance: Acceptance,
    pub eval_cases: Vec<EvalCase>,
}
