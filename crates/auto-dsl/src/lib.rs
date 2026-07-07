//! The extraction DSL, v0 — the closed program space symbolic extraction
//! searches over (spec/synthesis.md).
//!
//! A [`Program`] is a straight-line pipeline of typed [`Op`]s over a single
//! value register (initialized to the input). Deliberate properties:
//!
//! - **Closed and total**: no arbitrary code, no I/O, no randomness; every
//!   op either produces a value or fails typed ([`EvalError`]).
//! - **No input-equality branching**: the DSL cannot express memo-tables
//!   (`if input == x then …`), so a synthesized program that fits the
//!   observations is structurally forced to generalize — it cannot just
//!   replay them. (`ConstOut` exists for genuinely constant behavior and is
//!   only ever proposed when *all* observed outputs are identical.)
//! - **One implementation, two compilations**: this same evaluator runs
//!   natively inside the synthesizer and compiles to wasm inside the
//!   artifact interpreter, so native/wasm divergence is caught by the
//!   differential gate rather than papered over.
//!
//! Wire form: canonical JSON (`{"dsl_version":0,"ops":[…]}`, sorted keys) —
//! the `program.json` entry of synthesized artifacts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Program wire-format version; readers accept exactly this. Bump with an ADR.
pub const DSL_VERSION: u32 = 0;

/// One pipeline step. Type table (register in → register out):
///
/// | op | in | out |
/// |---|---|---|
/// | `GetField(k)` | object | field value |
/// | `Lowercase` / `Uppercase` / `Trim` | text | text |
/// | `SplitWhitespace` | text | list<text> |
/// | `SplitOn(sep)` | text | list<text> |
/// | `TrimEachMatches(set)` | list<text> | list<text> |
/// | `FilterLongerThan(n)` | list<text> | list<text> (keep chars > n) |
/// | `DedupSort` | list<text> | list<text> (unique, ascending) |
/// | `Take(n)` | list | list (first n) |
/// | `First` / `Last` | list | element |
/// | `Join(sep)` | list<text> | text |
/// | `Count` | list | int |
/// | `CharCount` | text | int |
/// | `Add(k)` | int | int |
/// | `ConstOut(v)` | anything | v |
/// Wire form is externally tagged (serde default) because strictness is
/// real there: unit ops serialize as bare strings (`"lowercase"`), ops with
/// fields as single-key objects (`{"get_field":{"key":"prompt"}}`), and
/// `deny_unknown_fields` actually rejects stray fields (it is silently
/// ineffective under internal tagging — a serde limitation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    GetField { key: String },
    Lowercase,
    Uppercase,
    Trim,
    SplitWhitespace,
    SplitOn { sep: String },
    TrimEachMatches { set: String },
    FilterLongerThan { n: u32 },
    DedupSort,
    Take { n: u32 },
    First,
    Last,
    Join { sep: String },
    Count,
    CharCount,
    Add { k: i64 },
    ConstOut { value: Value },
}

/// A straight-line pipeline. Empty programs are invalid (nothing observed
/// nothing claimed — an identity mapping is expressible but must be said,
/// which v0 has no op for; open questions).
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub ops: Vec<Op>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EvalError {
    #[error("op {index} ({op}): expected {expected}, register holds {found}")]
    TypeMismatch {
        index: usize,
        op: &'static str,
        expected: &'static str,
        found: &'static str,
    },
    #[error("op {index} (get_field): object has no key {key:?}")]
    MissingField { index: usize, key: String },
    #[error("op {index} (first/last): list is empty")]
    EmptyList { index: usize },
    #[error("op {index} (add): integer overflow")]
    Overflow { index: usize },
    #[error("program has no ops")]
    EmptyProgram,
}

#[derive(Debug, thiserror::Error)]
pub enum ProgramError {
    #[error("invalid program json: {0}")]
    BadJson(String),
    #[error("unsupported dsl_version {found}; this build reads exactly 0")]
    UnsupportedVersion { found: u32 },
    #[error("program has no ops")]
    Empty,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    dsl_version: u32,
    ops: Vec<Op>,
}

impl Program {
    pub fn new(ops: Vec<Op>) -> Self {
        Self { ops }
    }

    /// Canonical JSON (sorted keys — serde_json's default map is ordered).
    pub fn to_json(&self) -> String {
        let wire = Wire {
            dsl_version: DSL_VERSION,
            ops: self.ops.clone(),
        };
        let value = serde_json::to_value(&wire).expect("program serialization cannot fail");
        serde_json::to_string(&value).expect("value serialization cannot fail")
    }

    /// Strict parse: unknown fields/ops and other versions are rejected.
    pub fn from_json(text: &str) -> Result<Self, ProgramError> {
        let wire: Wire =
            serde_json::from_str(text).map_err(|e| ProgramError::BadJson(e.to_string()))?;
        if wire.dsl_version != DSL_VERSION {
            return Err(ProgramError::UnsupportedVersion {
                found: wire.dsl_version,
            });
        }
        if wire.ops.is_empty() {
            return Err(ProgramError::Empty);
        }
        Ok(Self { ops: wire.ops })
    }
}

/// Pipeline wire-format version ceiling; readers accept 0 (program stages
/// only) and 1 (program + tool stages). Writers emit the LOWEST version that
/// carries the pipeline: 0 when every stage is a program (byte-compatible
/// with wave-4 artifacts), 1 when any tool stage exists. Bump with an ADR.
pub const PIPELINE_VERSION: u32 = 1;

/// One pipeline step: a synthesized program, or a declared TOOL CALL — the
/// capability boundary (ADR-0017). A tool stage does not compute; it hands
/// the register to the host's `auto.tool_call` import as
/// `{"name":…,"input":<register>}` and continues with the returned value.
/// The tool names of a pipeline ARE the artifact's declared capabilities.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Program(Program),
    Tool { name: String },
}

/// A region artifact's payload: stages applied left-to-right, the output of
/// one feeding the input of the next (spec/synthesis.md §8). Identity glue
/// between stages is OMITTED, never expressed — the DSL has no identity
/// program (a program must have ops), so a pipeline lists only the
/// value-changing steps and the capability boundaries.
#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    pub stages: Vec<Stage>,
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("invalid pipeline json: {0}")]
    BadJson(String),
    #[error("unsupported pipeline_version {found}; this build reads 0 and 1")]
    UnsupportedVersion { found: u32 },
    #[error("pipeline has no stages")]
    Empty,
    #[error("pipeline stage #{index}: {error}")]
    BadStage { index: usize, error: String },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelineWireV0 {
    pipeline_version: u32,
    programs: Vec<Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelineWireV1 {
    pipeline_version: u32,
    stages: Vec<Value>,
}

impl Pipeline {
    pub fn new(stages: Vec<Stage>) -> Self {
        Self { stages }
    }

    /// A pure pipeline from programs only (the wave-4 form).
    pub fn from_programs(programs: Vec<Program>) -> Self {
        Self {
            stages: programs.into_iter().map(Stage::Program).collect(),
        }
    }

    /// The tool names this pipeline calls, sorted + deduplicated — exactly
    /// the artifact's declared capabilities.
    pub fn capabilities(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .stages
            .iter()
            .filter_map(|s| match s {
                Stage::Tool { name } => Some(name.clone()),
                Stage::Program(_) => None,
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Canonical JSON. Writers emit the LOWEST version that carries the
    /// pipeline: v0 when every stage is a program (wave-4 byte
    /// compatibility), v1 otherwise.
    pub fn to_json(&self) -> String {
        if self.capabilities().is_empty() {
            let programs: Vec<Value> = self
                .stages
                .iter()
                .map(|s| match s {
                    Stage::Program(p) => {
                        serde_json::from_str(&p.to_json()).expect("program json re-parses")
                    }
                    Stage::Tool { .. } => unreachable!("pure pipeline has no tools"),
                })
                .collect();
            let wire = PipelineWireV0 {
                pipeline_version: 0,
                programs,
            };
            let value = serde_json::to_value(&wire).expect("pipeline serialization cannot fail");
            return serde_json::to_string(&value).expect("value serialization cannot fail");
        }
        let stages: Vec<Value> = self
            .stages
            .iter()
            .map(|s| match s {
                Stage::Program(p) => {
                    let program: Value =
                        serde_json::from_str(&p.to_json()).expect("program json re-parses");
                    serde_json::json!({ "program": program })
                }
                Stage::Tool { name } => serde_json::json!({ "tool_call": { "name": name } }),
            })
            .collect();
        let wire = PipelineWireV1 {
            pipeline_version: 1,
            stages,
        };
        let value = serde_json::to_value(&wire).expect("pipeline serialization cannot fail");
        serde_json::to_string(&value).expect("value serialization cannot fail")
    }

    /// Strict parse of either wire version: exact known versions, unknown
    /// fields rejected, at least one stage, every element strict.
    pub fn from_json(text: &str) -> Result<Self, PipelineError> {
        let probe: Value =
            serde_json::from_str(text).map_err(|e| PipelineError::BadJson(e.to_string()))?;
        let version = probe
            .get("pipeline_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| PipelineError::BadJson("no integer pipeline_version".into()))?;
        match version {
            0 => {
                let wire: PipelineWireV0 = serde_json::from_str(text)
                    .map_err(|e| PipelineError::BadJson(e.to_string()))?;
                if wire.programs.is_empty() {
                    return Err(PipelineError::Empty);
                }
                let stages = wire
                    .programs
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        let text =
                            serde_json::to_string(value).expect("value serialization cannot fail");
                        Program::from_json(&text).map(Stage::Program).map_err(|e| {
                            PipelineError::BadStage {
                                index,
                                error: e.to_string(),
                            }
                        })
                    })
                    .collect::<Result<Vec<Stage>, PipelineError>>()?;
                Ok(Self { stages })
            }
            1 => {
                let wire: PipelineWireV1 = serde_json::from_str(text)
                    .map_err(|e| PipelineError::BadJson(e.to_string()))?;
                if wire.stages.is_empty() {
                    return Err(PipelineError::Empty);
                }
                let stages = wire
                    .stages
                    .iter()
                    .enumerate()
                    .map(|(index, value)| parse_stage(index, value))
                    .collect::<Result<Vec<Stage>, PipelineError>>()?;
                Ok(Self { stages })
            }
            other => Err(PipelineError::UnsupportedVersion {
                found: other as u32,
            }),
        }
    }
}

/// One v1 stage: exactly one of `program` / `tool_call`.
fn parse_stage(index: usize, value: &Value) -> Result<Stage, PipelineError> {
    let object = value.as_object().ok_or_else(|| PipelineError::BadStage {
        index,
        error: "stage must be an object".into(),
    })?;
    match (object.get("program"), object.get("tool_call")) {
        (Some(program), None) if object.len() == 1 => {
            let text = serde_json::to_string(program).expect("value serialization cannot fail");
            Program::from_json(&text)
                .map(Stage::Program)
                .map_err(|e| PipelineError::BadStage {
                    index,
                    error: e.to_string(),
                })
        }
        (None, Some(tool)) if object.len() == 1 => {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .ok_or_else(|| PipelineError::BadStage {
                    index,
                    error: "tool_call needs a non-empty name".into(),
                })?;
            if tool.as_object().map(|o| o.len()) != Some(1) {
                return Err(PipelineError::BadStage {
                    index,
                    error: "tool_call carries exactly {name}".into(),
                });
            }
            Ok(Stage::Tool {
                name: name.to_owned(),
            })
        }
        _ => Err(PipelineError::BadStage {
            index,
            error: "stage is exactly one of {program} | {tool_call}".into(),
        }),
    }
}

/// Run a pipeline: fold stages left-to-right. Program stages run [`eval`];
/// tool stages call `tool(name, register)` — the ONE seam both compilations
/// share: natively the gate supplies recorded replay (hermetic), in wasm the
/// interpreter's import calls the host (spec/artifact.md, ADR-0017). A tool
/// error is a stage-tagged failure.
pub fn eval_pipeline(
    pipeline: &Pipeline,
    input: &Value,
    tool: &mut dyn FnMut(&str, &Value) -> Result<Value, String>,
) -> Result<Value, PipelineEvalError> {
    let mut register = input.clone();
    for (stage, step) in pipeline.stages.iter().enumerate() {
        register = match step {
            Stage::Program(program) => {
                eval(program, &register).map_err(|error| PipelineEvalError {
                    stage,
                    error: error.to_string(),
                })?
            }
            Stage::Tool { name } => tool(name, &register).map_err(|error| PipelineEvalError {
                stage,
                error: format!("tool {name}: {error}"),
            })?,
        };
    }
    Ok(register)
}

/// A refusing tool seam for pure contexts: any tool stage is an error.
pub fn no_tools(name: &str, _input: &Value) -> Result<Value, String> {
    Err(format!(
        "tool {name:?} requested but no tool host is available (pure context)"
    ))
}

/// A stage-tagged pipeline failure (a program eval error or a tool error).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("pipeline stage #{stage}: {error}")]
pub struct PipelineEvalError {
    pub stage: usize,
    pub error: String,
}

/// An artifact's `program.json` payload: a single program (span artifacts)
/// or a pipeline (region artifacts). Sniffed strictly by the version key —
/// exactly one of `dsl_version` / `pipeline_version` must be present.
#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    Program(Program),
    Pipeline(Pipeline),
}

impl Payload {
    /// Strict parse by version-key sniff; anything ambiguous or alien is
    /// rejected loudly.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| format!("invalid payload json: {e}"))?;
        let Some(object) = value.as_object() else {
            return Err("payload must be a JSON object".to_owned());
        };
        match (
            object.contains_key("dsl_version"),
            object.contains_key("pipeline_version"),
        ) {
            (true, false) => Program::from_json(text)
                .map(Payload::Program)
                .map_err(|e| e.to_string()),
            (false, true) => Pipeline::from_json(text)
                .map(Payload::Pipeline)
                .map_err(|e| e.to_string()),
            (true, true) => Err("payload carries BOTH dsl_version and pipeline_version".to_owned()),
            (false, false) => {
                Err("payload carries neither dsl_version nor pipeline_version".to_owned())
            }
        }
    }

    /// Canonical JSON of whichever form this is.
    pub fn to_json(&self) -> String {
        match self {
            Payload::Program(p) => p.to_json(),
            Payload::Pipeline(p) => p.to_json(),
        }
    }
}

/// Evaluate either payload form — the single implementation both the native
/// search side and the wasm interpreter compile (spec/synthesis.md §5). The
/// tool seam is consulted only by tool stages; pure payloads never call it.
pub fn eval_payload(
    payload: &Payload,
    input: &Value,
    tool: &mut dyn FnMut(&str, &Value) -> Result<Value, String>,
) -> Result<Value, String> {
    match payload {
        Payload::Program(p) => eval(p, input).map_err(|e| e.to_string()),
        Payload::Pipeline(p) => eval_pipeline(p, input, tool).map_err(|e| e.to_string()),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "text",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

fn as_text_list(v: &Value) -> Option<Vec<&str>> {
    v.as_array()?
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()
}

fn text_list(items: Vec<String>) -> Value {
    Value::Array(items.into_iter().map(Value::String).collect())
}

/// Evaluate a program on an input. Total: every failure is a typed
/// [`EvalError`]; nothing panics.
///
/// Security note: despite the name, this is not code evaluation — `Op` is a
/// closed enum of pure data transformations (no I/O, no exec, no reflection),
/// so no program this function accepts can run arbitrary code. That
/// closedness is the point (spec/synthesis.md; ADR-0003 rejected
/// arbitrary-code predicates on the same principle).
pub fn eval(program: &Program, input: &Value) -> Result<Value, EvalError> {
    if program.ops.is_empty() {
        return Err(EvalError::EmptyProgram);
    }
    let mut register = input.clone();
    for (index, op) in program.ops.iter().enumerate() {
        register = step(index, op, register)?;
    }
    Ok(register)
}

#[expect(
    clippy::too_many_lines,
    reason = "one arm per op; splitting would obscure the type table"
)]
fn step(index: usize, op: &Op, register: Value) -> Result<Value, EvalError> {
    let mismatch =
        |op: &'static str, expected: &'static str, found: &Value| EvalError::TypeMismatch {
            index,
            op,
            expected,
            found: type_name(found),
        };
    Ok(match op {
        Op::GetField { key } => match &register {
            Value::Object(map) => map
                .get(key)
                .cloned()
                .ok_or_else(|| EvalError::MissingField {
                    index,
                    key: key.clone(),
                })?,
            other => return Err(mismatch("get_field", "object", other)),
        },
        Op::Lowercase => match &register {
            Value::String(s) => Value::String(s.to_lowercase()),
            other => return Err(mismatch("lowercase", "text", other)),
        },
        Op::Uppercase => match &register {
            Value::String(s) => Value::String(s.to_uppercase()),
            other => return Err(mismatch("uppercase", "text", other)),
        },
        Op::Trim => match &register {
            Value::String(s) => Value::String(s.trim().to_owned()),
            other => return Err(mismatch("trim", "text", other)),
        },
        Op::SplitWhitespace => match &register {
            Value::String(s) => text_list(s.split_whitespace().map(str::to_owned).collect()),
            other => return Err(mismatch("split_whitespace", "text", other)),
        },
        Op::SplitOn { sep } => match &register {
            Value::String(s) => text_list(s.split(sep.as_str()).map(str::to_owned).collect()),
            other => return Err(mismatch("split_on", "text", other)),
        },
        Op::TrimEachMatches { set } => match as_text_list(&register) {
            Some(items) => text_list(
                items
                    .iter()
                    .map(|w| w.trim_matches(|c| set.contains(c)).to_owned())
                    .collect(),
            ),
            None => return Err(mismatch("trim_each_matches", "list<text>", &register)),
        },
        Op::FilterLongerThan { n } => match as_text_list(&register) {
            Some(items) => text_list(
                items
                    .iter()
                    .filter(|w| w.chars().count() > *n as usize)
                    .map(|w| (*w).to_owned())
                    .collect(),
            ),
            None => return Err(mismatch("filter_longer_than", "list<text>", &register)),
        },
        Op::DedupSort => match as_text_list(&register) {
            Some(items) => {
                let set: std::collections::BTreeSet<String> =
                    items.into_iter().map(str::to_owned).collect();
                text_list(set.into_iter().collect())
            }
            None => return Err(mismatch("dedup_sort", "list<text>", &register)),
        },
        Op::Take { n } => match &register {
            Value::Array(items) => Value::Array(items.iter().take(*n as usize).cloned().collect()),
            other => return Err(mismatch("take", "list", other)),
        },
        Op::First => match &register {
            Value::Array(items) => items
                .first()
                .cloned()
                .ok_or(EvalError::EmptyList { index })?,
            other => return Err(mismatch("first", "list", other)),
        },
        Op::Last => match &register {
            Value::Array(items) => items
                .last()
                .cloned()
                .ok_or(EvalError::EmptyList { index })?,
            other => return Err(mismatch("last", "list", other)),
        },
        Op::Join { sep } => match as_text_list(&register) {
            Some(items) => Value::String(items.join(sep)),
            None => return Err(mismatch("join", "list<text>", &register)),
        },
        Op::Count => match &register {
            Value::Array(items) => Value::from(items.len() as u64),
            other => return Err(mismatch("count", "list", other)),
        },
        Op::CharCount => match &register {
            Value::String(s) => Value::from(s.chars().count() as u64),
            other => return Err(mismatch("char_count", "text", other)),
        },
        Op::Add { k } => match &register {
            Value::Number(n) => match n.as_i64() {
                Some(v) => Value::from(v.checked_add(*k).ok_or(EvalError::Overflow { index })?),
                None => return Err(mismatch("add", "int", &register)),
            },
            other => return Err(mismatch("add", "int", other)),
        },
        Op::ConstOut { value } => value.clone(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The fake-frontier extraction, spelled in the DSL — the S4 target.
    fn fake_frontier_program() -> Program {
        Program::new(vec![
            Op::GetField {
                key: "prompt".into(),
            },
            Op::Lowercase,
            Op::SplitWhitespace,
            Op::TrimEachMatches { set: ".,".into() },
            Op::FilterLongerThan { n: 4 },
            Op::DedupSort,
            Op::Take { n: 3 },
            Op::Join { sep: " ".into() },
        ])
    }

    #[test]
    fn fake_frontier_pipeline_matches_recorded_behavior() {
        let input =
            json!({"prompt": "The quick brown fox jumps over the lazy dog near the riverbank."});
        assert_eq!(
            eval(&fake_frontier_program(), &input),
            Ok(json!("brown jumps quick"))
        );
    }

    #[test]
    fn wordcount_pipeline() {
        let p = Program::new(vec![
            Op::GetField { key: "text".into() },
            Op::SplitWhitespace,
            Op::Count,
        ]);
        assert_eq!(eval(&p, &json!({"text": "a b c"})), Ok(json!(3)));
    }

    #[test]
    fn type_errors_are_typed_not_panics() {
        let p = Program::new(vec![Op::Lowercase]);
        assert!(matches!(
            eval(&p, &json!(42)),
            Err(EvalError::TypeMismatch {
                op: "lowercase",
                ..
            })
        ));
        let p = Program::new(vec![Op::GetField { key: "x".into() }]);
        assert!(matches!(
            eval(&p, &json!({})),
            Err(EvalError::MissingField { .. })
        ));
        let p = Program::new(vec![Op::First]);
        assert!(matches!(
            eval(&p, &json!([])),
            Err(EvalError::EmptyList { .. })
        ));
        assert_eq!(
            eval(&Program::new(vec![]), &json!(1)),
            Err(EvalError::EmptyProgram)
        );
    }

    #[test]
    fn add_overflow_is_an_error() {
        let p = Program::new(vec![Op::Add { k: 1 }]);
        assert!(matches!(
            eval(&p, &json!(i64::MAX)),
            Err(EvalError::Overflow { .. })
        ));
    }

    #[test]
    fn json_roundtrip_is_strict_and_canonical() {
        let p = fake_frontier_program();
        let text = p.to_json();
        assert!(text.starts_with("{\"dsl_version\":0,\"ops\":["));
        assert_eq!(Program::from_json(&text).unwrap(), p);

        // externally tagged: unit ops are bare strings, field ops single-key objects
        assert!(text.contains("\"lowercase\""));
        assert!(text.contains("{\"get_field\":{\"key\":\"prompt\"}}"));

        assert!(matches!(
            Program::from_json("{\"dsl_version\":1,\"ops\":[\"trim\"]}"),
            Err(ProgramError::UnsupportedVersion { found: 1 })
        ));
        assert!(matches!(
            Program::from_json("{\"dsl_version\":0,\"ops\":[]}"),
            Err(ProgramError::Empty)
        ));
        assert!(matches!(
            Program::from_json("{\"dsl_version\":0,\"ops\":[\"warp_speed\"]}"),
            Err(ProgramError::BadJson(_))
        ));
        assert!(matches!(
            Program::from_json(
                "{\"dsl_version\":0,\"ops\":[{\"get_field\":{\"key\":\"a\",\"extra\":1}}]}"
            ),
            Err(ProgramError::BadJson(_))
        ));
    }

    #[test]
    fn const_out_ignores_register() {
        let p = Program::new(vec![Op::ConstOut { value: json!("x") }]);
        assert_eq!(eval(&p, &json!({"anything": 1})), Ok(json!("x")));
    }
}

#[cfg(test)]
mod pipeline_tests {
    use serde_json::json;

    use super::*;

    fn extract() -> Program {
        Program::new(vec![
            Op::GetField { key: "doc".into() },
            Op::Lowercase,
            Op::SplitWhitespace,
            Op::Join { sep: " ".into() },
        ])
    }

    fn upper() -> Program {
        Program::new(vec![Op::Uppercase])
    }

    #[test]
    fn pure_pipeline_round_trips_as_v0_canonically() {
        let pipeline = Pipeline::from_programs(vec![extract(), upper()]);
        let json = pipeline.to_json();
        assert!(json.contains("\"pipeline_version\":0"), "{json}");
        assert!(json.contains("\"programs\""), "wave-4 wire form preserved");
        let back = Pipeline::from_json(&json).expect("canonical json re-parses");
        assert_eq!(back, pipeline);
        assert_eq!(back.to_json(), json, "canonical form is a fixed point");
        assert!(pipeline.capabilities().is_empty());
    }

    #[test]
    fn tool_pipeline_round_trips_as_v1_and_names_its_capabilities() {
        let pipeline = Pipeline::new(vec![
            Stage::Program(extract()),
            Stage::Tool {
                name: "lookup".into(),
            },
            Stage::Program(upper()),
        ]);
        let json = pipeline.to_json();
        assert!(json.contains("\"pipeline_version\":1"), "{json}");
        assert!(json.contains("\"tool_call\""), "{json}");
        let back = Pipeline::from_json(&json).expect("v1 re-parses");
        assert_eq!(back, pipeline);
        assert_eq!(back.to_json(), json);
        assert_eq!(pipeline.capabilities(), vec!["lookup".to_owned()]);
    }

    #[test]
    fn pipeline_composes_left_to_right() {
        let pipeline = Pipeline::from_programs(vec![extract(), upper()]);
        assert_eq!(
            eval_pipeline(&pipeline, &json!({"doc": "Hello  World"}), &mut no_tools),
            Ok(json!("HELLO WORLD"))
        );
    }

    #[test]
    fn tool_stages_consult_the_seam_and_pure_contexts_refuse() {
        let pipeline = Pipeline::new(vec![
            Stage::Program(extract()),
            Stage::Tool {
                name: "lookup".into(),
            },
        ]);
        // a labeled test seam, not a mock pretending to be a tool
        let mut seam = |name: &str, input: &Value| -> Result<Value, String> {
            assert_eq!(name, "lookup");
            Ok(json!(format!("routed:{}", input.as_str().unwrap())))
        };
        assert_eq!(
            eval_pipeline(&pipeline, &json!({"doc": "Beta Alpha"}), &mut seam),
            Ok(json!("routed:beta alpha"))
        );
        let err = eval_pipeline(&pipeline, &json!({"doc": "x"}), &mut no_tools)
            .expect_err("pure contexts refuse tool stages");
        assert_eq!(err.stage, 1);
        assert!(err.to_string().contains("no tool host"), "{err}");
    }

    #[test]
    fn pipeline_eval_error_names_the_stage() {
        let pipeline = Pipeline::from_programs(vec![
            Program::new(vec![
                Op::GetField { key: "doc".into() },
                Op::SplitWhitespace,
            ]),
            upper(),
        ]);
        let err = eval_pipeline(&pipeline, &json!({"doc": "a b"}), &mut no_tools)
            .expect_err("type mismatch");
        assert_eq!(err.stage, 1);
        assert!(err.to_string().contains("stage #1"), "{err}");
    }

    #[test]
    fn strict_parse_rejections() {
        assert!(matches!(
            Pipeline::from_json("{\"pipeline_version\":2,\"stages\":[]}"),
            Err(PipelineError::UnsupportedVersion { found: 2 })
        ));
        assert!(matches!(
            Pipeline::from_json("{\"pipeline_version\":0,\"programs\":[]}"),
            Err(PipelineError::Empty)
        ));
        assert!(matches!(
            Pipeline::from_json("{\"pipeline_version\":1,\"stages\":[]}"),
            Err(PipelineError::Empty)
        ));
        let bad = "{\"pipeline_version\":0,\"programs\":[{\"dsl_version\":0,\"ops\":[]}]}";
        assert!(matches!(
            Pipeline::from_json(bad),
            Err(PipelineError::BadStage { index: 0, .. })
        ));
        // v1 stage strictness: both keys, neither key, empty tool name
        for bad in [
            "{\"pipeline_version\":1,\"stages\":[{\"program\":{\"dsl_version\":0,\"ops\":[\"trim\"]},\"tool_call\":{\"name\":\"x\"}}]}",
            "{\"pipeline_version\":1,\"stages\":[{}]}",
            "{\"pipeline_version\":1,\"stages\":[{\"tool_call\":{\"name\":\"\"}}]}",
            "{\"pipeline_version\":1,\"stages\":[{\"tool_call\":{\"name\":\"x\",\"extra\":1}}]}",
        ] {
            assert!(
                matches!(
                    Pipeline::from_json(bad),
                    Err(PipelineError::BadStage { .. })
                ),
                "{bad}"
            );
        }
        assert!(
            Pipeline::from_json("{\"pipeline_version\":0,\"programs\":[],\"extra\":1}").is_err()
        );
    }

    #[test]
    fn payload_sniffs_strictly_by_version_key() {
        let program_json = extract().to_json();
        assert!(matches!(
            Payload::from_json(&program_json),
            Ok(Payload::Program(_))
        ));
        let pipeline_json = Pipeline::from_programs(vec![extract()]).to_json();
        assert!(matches!(
            Payload::from_json(&pipeline_json),
            Ok(Payload::Pipeline(_))
        ));
        assert!(Payload::from_json("{\"neither\":true}").is_err());
        assert!(
            Payload::from_json("{\"dsl_version\":0,\"pipeline_version\":0,\"ops\":[]}").is_err()
        );
        assert!(Payload::from_json("[1,2]").is_err());
    }

    #[test]
    fn eval_payload_covers_both_forms() {
        let input = json!({"doc": "Ping Pong"});
        let as_program = Payload::Program(extract());
        assert_eq!(
            eval_payload(&as_program, &input, &mut no_tools),
            Ok(json!("ping pong"))
        );
        let as_pipeline = Payload::Pipeline(Pipeline::from_programs(vec![extract(), upper()]));
        assert_eq!(
            eval_payload(&as_pipeline, &input, &mut no_tools),
            Ok(json!("PING PONG"))
        );
    }
}
