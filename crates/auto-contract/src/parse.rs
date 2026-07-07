//! Strict contract loading: TOML text -> [`Contract`], plus referenced
//! eval-set JSONL files and the content-addressed contract id.
//!
//! Strict means: unknown keys are rejected everywhere, property field sets
//! are closed per kind, example names are unique, TOML datetimes and
//! non-finite numbers are rejected. Format spec: `spec/contract.md`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use auto_ir::ValueType;
use serde_json::{Map, Value};

use crate::model::{
    Acceptance, Budgets, CONTRACT_VERSION, Contract, ContractId, DifferentialMatch, EvalCase,
    Example, Interface, MatchMode, Property, Scope, Target,
};

/// Span kinds a `type = "span"` scope may bind to (spec/trace.md §2).
const SPAN_KINDS: [&str; 5] = ["model_call", "tool_call", "env_read", "memory_op", "branch"];

/// Everything strict loading can reject.
#[derive(Debug, thiserror::Error)]
pub enum ContractError {
    /// the contract file could not be read
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// TOML syntax error, or a missing / unknown / mistyped key
    #[error("toml: {message}")]
    Toml { message: String },
    /// wire-format version this build does not read
    #[error("unsupported contract_version {found}")]
    UnsupportedVersion { found: u32 },
    /// a field that must be non-empty was empty
    #[error("`{field}` must be non-empty")]
    EmptyField { field: &'static str },
    /// `[scope] type` outside the v0 set (`task`, `span`)
    #[error("unknown scope type `{0}`")]
    UnknownScopeType(String),
    /// `[scope] kind` outside the effectful span kinds
    #[error("unknown span kind `{0}`")]
    UnknownSpanKind(String),
    /// scope keys inconsistent with the scope type
    #[error("scope: {detail}")]
    ScopeFieldMismatch { detail: String },
    /// interface type text outside the v0 type grammar
    #[error("`{field}`: `{text}` is not a v0 type")]
    BadValueType { field: String, text: String },
    /// example `match` outside the v0 set (`exact`, `judged`)
    #[error("unknown match mode `{0}`")]
    UnknownMatchMode(String),
    /// two examples share a name
    #[error("duplicate example name `{0}`")]
    DuplicateExampleName(String),
    /// property `target` outside the v0 set (`output`)
    #[error("unknown property target `{0}`")]
    UnknownTarget(String),
    /// property `kind` outside the v0 property language
    #[error("unknown property kind `{0}`")]
    UnknownPropertyKind(String),
    /// a property field outside its kind's closed field set
    #[error("property kind `{kind}` takes no field `{field}`")]
    UnknownPropertyField { kind: String, field: String },
    /// bounds missing entirely or inverted
    #[error("invalid range: {detail}")]
    InvalidRange { detail: String },
    /// regex property pattern failed to compile
    #[error("bad regex `{pattern}`: {error}")]
    BadRegex { pattern: String, error: String },
    /// TOML datetimes have no JSON form and are not part of v0
    #[error("datetime not supported at {location}")]
    DatetimeNotSupported { location: String },
    /// inf / nan have no JSON form
    #[error("non-finite number at {location}")]
    NonFiniteNumber { location: String },
    /// a referenced eval-set file has a bad line (1-based line number)
    #[error("eval set {}, line {line}: {why}", .path.display())]
    BadEvalSet {
        path: PathBuf,
        line: usize,
        why: String,
    },
    /// a referenced eval-set file could not be read
    #[error("eval set {}: {error}", .path.display())]
    EvalSetIo { path: PathBuf, error: String },
}

/// Load a contract from a TOML file. `eval_sets` paths resolve relative to
/// the file's directory.
pub fn load(path: &Path) -> Result<Contract, ContractError> {
    let text = std::fs::read_to_string(path)?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new(""));
    from_toml_str(&text, base_dir)
}

/// Parse contract TOML. `base_dir` anchors relative `eval_sets` paths.
pub fn from_toml_str(text: &str, base_dir: &Path) -> Result<Contract, ContractError> {
    let mut root: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| ContractError::Toml {
            message: e.to_string(),
        })?;

    let found = expect_u32(
        &require(&mut root, "contract_version", "top level")?,
        "`contract_version`",
    )?;
    if found != CONTRACT_VERSION {
        return Err(ContractError::UnsupportedVersion { found });
    }

    let task = expect_string(require(&mut root, "task", "top level")?, "`task`")?;
    if task.is_empty() {
        return Err(ContractError::EmptyField { field: "task" });
    }

    let scope = parse_scope(require(&mut root, "scope", "top level")?)?;
    let interface = parse_interface(require(&mut root, "interface", "top level")?)?;
    let examples = match root.remove("example") {
        Some(value) => parse_examples(value)?,
        None => Vec::new(),
    };
    let properties = match root.remove("property") {
        Some(value) => parse_properties(value)?,
        None => Vec::new(),
    };
    let budgets = match root.remove("budgets") {
        Some(value) => parse_budgets(value)?,
        None => Budgets::default(),
    };
    let acceptance = match root.remove("acceptance") {
        Some(value) => parse_acceptance(value)?,
        None => Acceptance::default(),
    };
    let eval_set_paths = match root.remove("eval_sets") {
        Some(value) => parse_eval_set_paths(value, base_dir)?,
        None => Vec::new(),
    };
    no_extra_keys(&root, "top level")?;

    let mut eval_cases = Vec::new();
    for path in &eval_set_paths {
        eval_cases.extend(load_eval_set(path)?);
    }

    Ok(Contract {
        task,
        scope,
        interface,
        examples,
        properties,
        budgets,
        acceptance,
        eval_cases,
    })
}

/// Parse the v0 type grammar `unit|bool|int|float|text|bytes|json|list<T>`
/// (nested lists allowed; exact lowercase; no whitespace). Inverse of
/// [`ValueType`]'s `Display`. Public: manifests carry interface types as
/// these strings and the runtime re-parses them.
pub fn parse_value_type(s: &str) -> Option<ValueType> {
    match s {
        "unit" => Some(ValueType::Unit),
        "bool" => Some(ValueType::Bool),
        "int" => Some(ValueType::Int),
        "float" => Some(ValueType::Float),
        "text" => Some(ValueType::Text),
        "bytes" => Some(ValueType::Bytes),
        "json" => Some(ValueType::Json),
        _ => {
            let elem = s.strip_prefix("list<")?.strip_suffix('>')?;
            Some(ValueType::List(Box::new(parse_value_type(elem)?)))
        }
    }
}

impl Contract {
    /// Canonical JSON form: one line, object keys sorted, absent optional
    /// fields omitted (never `null`, except an eval case whose expected
    /// output is JSON `null`). This string is the preimage of
    /// [`Contract::id`]. Panics if a `NumRange` bound is non-finite;
    /// `parse` never produces one.
    pub fn canonical_json(&self) -> String {
        let mut root = Map::new();
        if let Some(acceptance) = acceptance_json(&self.acceptance) {
            root.insert("acceptance".to_string(), acceptance);
        }
        root.insert("budgets".to_string(), budgets_json(&self.budgets));
        root.insert(
            "contract_version".to_string(),
            Value::from(CONTRACT_VERSION),
        );
        root.insert(
            "eval_cases".to_string(),
            Value::Array(self.eval_cases.iter().map(eval_case_json).collect()),
        );
        root.insert(
            "examples".to_string(),
            Value::Array(self.examples.iter().map(example_json).collect()),
        );
        root.insert("interface".to_string(), interface_json(&self.interface));
        root.insert(
            "properties".to_string(),
            Value::Array(self.properties.iter().map(property_json).collect()),
        );
        root.insert("scope".to_string(), scope_json(&self.scope));
        root.insert("task".to_string(), Value::String(self.task.clone()));
        auto_trace::model::canonical_json(&Value::Object(root))
    }

    /// Content address: lowercase-hex sha-256 of [`Contract::canonical_json`].
    pub fn id(&self) -> ContractId {
        ContractId(auto_trace::model::digest_hex(&self.canonical_json()))
    }
}

// ---- TOML walking ----------------------------------------------------------

fn toml_err(message: impl Into<String>) -> ContractError {
    ContractError::Toml {
        message: message.into(),
    }
}

fn require(table: &mut toml::Table, key: &str, ctx: &str) -> Result<toml::Value, ContractError> {
    table
        .remove(key)
        .ok_or_else(|| toml_err(format!("{ctx}: missing required key `{key}`")))
}

fn no_extra_keys(table: &toml::Table, ctx: &str) -> Result<(), ContractError> {
    match table.keys().next() {
        None => Ok(()),
        Some(key) => Err(toml_err(format!("{ctx}: unknown key `{key}`"))),
    }
}

fn expect_string(value: toml::Value, what: &str) -> Result<String, ContractError> {
    match value {
        toml::Value::String(s) => Ok(s),
        other => Err(toml_err(format!(
            "{what} must be a string, got {}",
            other.type_str()
        ))),
    }
}

fn expect_table(value: toml::Value, what: &str) -> Result<toml::Table, ContractError> {
    match value {
        toml::Value::Table(t) => Ok(t),
        other => Err(toml_err(format!(
            "{what} must be a table, got {}",
            other.type_str()
        ))),
    }
}

fn expect_array(value: toml::Value, what: &str) -> Result<Vec<toml::Value>, ContractError> {
    match value {
        toml::Value::Array(items) => Ok(items),
        other => Err(toml_err(format!(
            "{what} must be an array, got {}",
            other.type_str()
        ))),
    }
}

fn expect_u32(value: &toml::Value, what: &str) -> Result<u32, ContractError> {
    let toml::Value::Integer(n) = value else {
        return Err(toml_err(format!(
            "{what} must be an integer, got {}",
            value.type_str()
        )));
    };
    u32::try_from(*n).map_err(|_| toml_err(format!("{what} must fit an unsigned 32-bit integer")))
}

fn expect_u64(value: &toml::Value, what: &str) -> Result<u64, ContractError> {
    let toml::Value::Integer(n) = value else {
        return Err(toml_err(format!(
            "{what} must be an integer, got {}",
            value.type_str()
        )));
    };
    u64::try_from(*n).map_err(|_| toml_err(format!("{what} must be non-negative")))
}

fn take_u64(table: &mut toml::Table, key: &str, ctx: &str) -> Result<Option<u64>, ContractError> {
    let Some(value) = table.remove(key) else {
        return Ok(None);
    };
    Ok(Some(expect_u64(&value, &format!("{ctx} `{key}`"))?))
}

fn take_f64(table: &mut toml::Table, key: &str, ctx: &str) -> Result<Option<f64>, ContractError> {
    let Some(value) = table.remove(key) else {
        return Ok(None);
    };
    let location = format!("{ctx} `{key}`");
    let n = match value {
        // i64 -> f64 loses precision beyond 2^53; documented on `NumRange`.
        toml::Value::Integer(n) => n as f64,
        toml::Value::Float(f) => f,
        other => {
            return Err(toml_err(format!(
                "{location} must be a number, got {}",
                other.type_str()
            )));
        }
    };
    if !n.is_finite() {
        return Err(ContractError::NonFiniteNumber { location });
    }
    Ok(Some(n))
}

/// TOML value -> JSON value. Integers stay i64, floats must be finite,
/// datetimes are rejected. `location` names the value in error messages.
fn toml_to_json(value: toml::Value, location: &str) -> Result<Value, ContractError> {
    match value {
        toml::Value::String(s) => Ok(Value::String(s)),
        toml::Value::Integer(n) => Ok(Value::Number(n.into())),
        toml::Value::Float(f) => match serde_json::Number::from_f64(f) {
            Some(n) => Ok(Value::Number(n)),
            None => Err(ContractError::NonFiniteNumber {
                location: location.to_string(),
            }),
        },
        toml::Value::Boolean(b) => Ok(Value::Bool(b)),
        toml::Value::Datetime(_) => Err(ContractError::DatetimeNotSupported {
            location: location.to_string(),
        }),
        toml::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.into_iter().enumerate() {
                out.push(toml_to_json(item, &format!("{location}[{i}]"))?);
            }
            Ok(Value::Array(out))
        }
        toml::Value::Table(table) => {
            let mut out = Map::new();
            for (key, item) in table {
                let converted = toml_to_json(item, &format!("{location}.{key}"))?;
                out.insert(key, converted);
            }
            Ok(Value::Object(out))
        }
    }
}

// ---- sections --------------------------------------------------------------

fn parse_scope(value: toml::Value) -> Result<Scope, ContractError> {
    let mut t = expect_table(value, "[scope]")?;
    let scope_type = expect_string(require(&mut t, "type", "[scope]")?, "[scope] `type`")?;
    let kind = t.remove("kind");
    let name = t.remove("name");
    let from = t.remove("from");
    let to = t.remove("to");
    no_extra_keys(&t, "[scope]")?;
    if scope_type != "region" && (from.is_some() || to.is_some()) {
        return Err(ContractError::ScopeFieldMismatch {
            detail: format!("scope type `{scope_type}` takes no `from` or `to`"),
        });
    }
    match scope_type.as_str() {
        "task" => {
            if kind.is_some() || name.is_some() {
                return Err(ContractError::ScopeFieldMismatch {
                    detail: "scope type `task` takes no `kind` or `name`".to_string(),
                });
            }
            Ok(Scope::Task)
        }
        "span" => {
            let (Some(kind), Some(name)) = (kind, name) else {
                return Err(ContractError::ScopeFieldMismatch {
                    detail: "scope type `span` requires both `kind` and `name`".to_string(),
                });
            };
            let kind = expect_string(kind, "[scope] `kind`")?;
            if !SPAN_KINDS.contains(&kind.as_str()) {
                return Err(ContractError::UnknownSpanKind(kind));
            }
            let name = expect_string(name, "[scope] `name`")?;
            if name.is_empty() {
                return Err(ContractError::EmptyField {
                    field: "scope.name",
                });
            }
            Ok(Scope::Span { kind, name })
        }
        "region" => {
            if kind.is_some() || name.is_some() {
                return Err(ContractError::ScopeFieldMismatch {
                    detail: "scope type `region` takes `from` and `to`, not `kind`/`name`"
                        .to_string(),
                });
            }
            let (Some(from), Some(to)) = (from, to) else {
                return Err(ContractError::ScopeFieldMismatch {
                    detail: "scope type `region` requires both `from` and `to`".to_string(),
                });
            };
            let from = expect_string(from, "[scope] `from`")?;
            let to = expect_string(to, "[scope] `to`")?;
            if from.is_empty() {
                return Err(ContractError::EmptyField {
                    field: "scope.from",
                });
            }
            if to.is_empty() {
                return Err(ContractError::EmptyField { field: "scope.to" });
            }
            if from == to {
                return Err(ContractError::ScopeFieldMismatch {
                    detail: "scope `from` and `to` must name different spans (a single-span \
                             region IS a span scope)"
                        .to_string(),
                });
            }
            Ok(Scope::Region { from, to })
        }
        other => Err(ContractError::UnknownScopeType(other.to_string())),
    }
}

fn parse_interface(value: toml::Value) -> Result<Interface, ContractError> {
    let mut t = expect_table(value, "[interface]")?;
    let input = interface_value_type(&mut t, "input")?;
    let output = interface_value_type(&mut t, "output")?;
    no_extra_keys(&t, "[interface]")?;
    Ok(Interface { input, output })
}

fn interface_value_type(
    table: &mut toml::Table,
    key: &'static str,
) -> Result<ValueType, ContractError> {
    let text = expect_string(
        require(table, key, "[interface]")?,
        &format!("[interface] `{key}`"),
    )?;
    match parse_value_type(&text) {
        Some(value_type) => Ok(value_type),
        None => Err(ContractError::BadValueType {
            field: format!("interface.{key}"),
            text,
        }),
    }
}

fn parse_examples(value: toml::Value) -> Result<Vec<Example>, ContractError> {
    let items = expect_array(value, "`example`")?;
    let mut seen = BTreeSet::new();
    let mut examples = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let ctx = format!("example[{i}]");
        let mut t = expect_table(item, &ctx)?;
        let name = expect_string(require(&mut t, "name", &ctx)?, &format!("{ctx} `name`"))?;
        if name.is_empty() {
            return Err(ContractError::EmptyField {
                field: "example.name",
            });
        }
        if !seen.insert(name.clone()) {
            return Err(ContractError::DuplicateExampleName(name));
        }
        let mode = expect_string(require(&mut t, "match", &ctx)?, &format!("{ctx} `match`"))?;
        let match_mode = match mode.as_str() {
            "exact" => MatchMode::Exact,
            "judged" => MatchMode::Judged,
            _ => return Err(ContractError::UnknownMatchMode(mode)),
        };
        let input = toml_to_json(require(&mut t, "input", &ctx)?, &format!("{ctx}.input"))?;
        let output = toml_to_json(require(&mut t, "output", &ctx)?, &format!("{ctx}.output"))?;
        no_extra_keys(&t, &ctx)?;
        examples.push(Example {
            name,
            input,
            output,
            match_mode,
        });
    }
    Ok(examples)
}

fn parse_properties(value: toml::Value) -> Result<Vec<Property>, ContractError> {
    let items = expect_array(value, "`property`")?;
    let mut properties = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        properties.push(parse_property(item, i)?);
    }
    Ok(properties)
}

fn parse_property(value: toml::Value, i: usize) -> Result<Property, ContractError> {
    let ctx = format!("property[{i}]");
    let mut t = expect_table(value, &ctx)?;
    let kind = expect_string(require(&mut t, "kind", &ctx)?, &format!("{ctx} `kind`"))?;
    let target_text = expect_string(require(&mut t, "target", &ctx)?, &format!("{ctx} `target`"))?;
    if target_text != "output" {
        return Err(ContractError::UnknownTarget(target_text));
    }
    let target = Target::Output;
    let property = match kind.as_str() {
        "len_range" => {
            let min = take_u64(&mut t, "min", &ctx)?;
            let max = take_u64(&mut t, "max", &ctx)?;
            no_extra_property_fields(&t, "len_range")?;
            check_range(min, max, &ctx, "len_range")?;
            Property::LenRange { target, min, max }
        }
        "regex" => {
            let pattern = expect_string(
                require(&mut t, "pattern", &ctx)?,
                &format!("{ctx} `pattern`"),
            )?;
            no_extra_property_fields(&t, "regex")?;
            if let Err(error) = regex::Regex::new(&pattern) {
                return Err(ContractError::BadRegex {
                    pattern,
                    error: error.to_string(),
                });
            }
            Property::Regex { target, pattern }
        }
        "num_range" => {
            let min = take_f64(&mut t, "min", &ctx)?;
            let max = take_f64(&mut t, "max", &ctx)?;
            no_extra_property_fields(&t, "num_range")?;
            check_range(min, max, &ctx, "num_range")?;
            Property::NumRange { target, min, max }
        }
        "json_has_keys" => {
            let keys_value = require(&mut t, "keys", &ctx)?;
            no_extra_property_fields(&t, "json_has_keys")?;
            let items = expect_array(keys_value, &format!("{ctx} `keys`"))?;
            if items.is_empty() {
                return Err(ContractError::EmptyField {
                    field: "property.json_has_keys.keys",
                });
            }
            let mut keys = Vec::with_capacity(items.len());
            for (j, item) in items.into_iter().enumerate() {
                let key = expect_string(item, &format!("{ctx} keys[{j}]"))?;
                if key.is_empty() {
                    return Err(ContractError::EmptyField {
                        field: "property.json_has_keys.keys",
                    });
                }
                keys.push(key);
            }
            Property::JsonHasKeys { target, keys }
        }
        "one_of" => {
            let values_value = require(&mut t, "values", &ctx)?;
            no_extra_property_fields(&t, "one_of")?;
            let items = expect_array(values_value, &format!("{ctx} `values`"))?;
            if items.is_empty() {
                return Err(ContractError::EmptyField {
                    field: "property.one_of.values",
                });
            }
            let mut values = Vec::with_capacity(items.len());
            for (j, item) in items.into_iter().enumerate() {
                values.push(toml_to_json(item, &format!("{ctx}.values[{j}]"))?);
            }
            Property::OneOf { target, values }
        }
        other => return Err(ContractError::UnknownPropertyKind(other.to_string())),
    };
    Ok(property)
}

fn no_extra_property_fields(table: &toml::Table, kind: &str) -> Result<(), ContractError> {
    match table.keys().next() {
        None => Ok(()),
        Some(field) => Err(ContractError::UnknownPropertyField {
            kind: kind.to_string(),
            field: field.clone(),
        }),
    }
}

fn check_range<T: PartialOrd + Copy>(
    min: Option<T>,
    max: Option<T>,
    ctx: &str,
    kind: &str,
) -> Result<(), ContractError> {
    if min.is_none() && max.is_none() {
        return Err(ContractError::InvalidRange {
            detail: format!("{ctx}: {kind} requires at least one of `min`/`max`"),
        });
    }
    if let (Some(lo), Some(hi)) = (min, max)
        && lo > hi
    {
        return Err(ContractError::InvalidRange {
            detail: format!("{ctx}: {kind} `min` exceeds `max`"),
        });
    }
    Ok(())
}

fn parse_budgets(value: toml::Value) -> Result<Budgets, ContractError> {
    let mut t = expect_table(value, "[budgets]")?;
    let budgets = Budgets {
        max_latency_ms_p95: take_u64(&mut t, "max_latency_ms_p95", "[budgets]")?,
        max_cost_usd_micros: take_u64(&mut t, "max_cost_usd_micros", "[budgets]")?,
        max_tokens: take_u64(&mut t, "max_tokens", "[budgets]")?,
    };
    no_extra_keys(&t, "[budgets]")?;
    Ok(budgets)
}

/// `[acceptance]` (ADR-0018, ADR-0021). Every key optional; absent table =
/// exact. `differential_min_agreement_milli` must be an integer in
/// `1..=1000` — 1000 is declared-exact and legal; 0 (a vacuous gate) and
/// anything above 1000 are rejected loudly, as are unknown keys and
/// non-integers. `differential_match` is `"exact"` (the default) or
/// `"judged"`; anything else is rejected naming the two values, and
/// `"judged"` without a declared threshold is rejected — the ADR-0018
/// threshold is what a judged differential decides against.
fn parse_acceptance(value: toml::Value) -> Result<Acceptance, ContractError> {
    let mut t = expect_table(value, "[acceptance]")?;
    let differential_min_agreement_milli = match t.remove("differential_min_agreement_milli") {
        None => None,
        Some(value) => {
            let milli = expect_u32(&value, "[acceptance] `differential_min_agreement_milli`")?;
            if !(1..=1000).contains(&milli) {
                return Err(toml_err(format!(
                    "[acceptance] `differential_min_agreement_milli` must be in 1..=1000 \
                     thousandths (1000 = declared-exact), got {milli}"
                )));
            }
            Some(milli)
        }
    };
    let differential_match = match t.remove("differential_match") {
        None => DifferentialMatch::default(),
        Some(value) => {
            let mode = expect_string(value, "[acceptance] `differential_match`")?;
            match mode.as_str() {
                "exact" => DifferentialMatch::Exact,
                "judged" => DifferentialMatch::Judged,
                other => {
                    return Err(toml_err(format!(
                        "[acceptance] `differential_match` must be \"exact\" or \"judged\", \
                         got `{other}`"
                    )));
                }
            }
        }
    };
    if differential_match == DifferentialMatch::Judged && differential_min_agreement_milli.is_none()
    {
        return Err(toml_err(
            "[acceptance] `differential_match = \"judged\"` requires \
             `differential_min_agreement_milli` — the declared ADR-0018 threshold is what \
             a judged differential decides against (ADR-0021)",
        ));
    }
    no_extra_keys(&t, "[acceptance]")?;
    Ok(Acceptance {
        differential_min_agreement_milli,
        differential_match,
    })
}

// ---- eval sets -------------------------------------------------------------

fn parse_eval_set_paths(
    value: toml::Value,
    base_dir: &Path,
) -> Result<Vec<PathBuf>, ContractError> {
    let items = expect_array(value, "`eval_sets`")?;
    let mut paths = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let rel = expect_string(item, &format!("eval_sets[{i}]"))?;
        paths.push(base_dir.join(rel));
    }
    Ok(paths)
}

fn load_eval_set(path: &Path) -> Result<Vec<EvalCase>, ContractError> {
    let text = std::fs::read_to_string(path).map_err(|e| ContractError::EvalSetIo {
        path: path.to_path_buf(),
        error: e.to_string(),
    })?;
    let mut cases = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        cases.push(parse_eval_case(line, path, index + 1)?);
    }
    Ok(cases)
}

/// One JSONL line: a JSON object with required `input`, optional `expected`,
/// nothing else. `"expected": null` means an expected output of JSON `null`,
/// distinct from an absent `expected`.
fn parse_eval_case(line: &str, path: &Path, line_no: usize) -> Result<EvalCase, ContractError> {
    let bad = |why: String| ContractError::BadEvalSet {
        path: path.to_path_buf(),
        line: line_no,
        why,
    };
    let value: Value = serde_json::from_str(line).map_err(|e| bad(format!("invalid json: {e}")))?;
    let Value::Object(mut fields) = value else {
        return Err(bad("not a json object".to_string()));
    };
    let Some(input) = fields.remove("input") else {
        return Err(bad("missing required field `input`".to_string()));
    };
    let expected = fields.remove("expected");
    if let Some(field) = fields.keys().next() {
        return Err(bad(format!("unknown field `{field}`")));
    }
    Ok(EvalCase { input, expected })
}

// ---- canonical form --------------------------------------------------------

/// Acceptance is id-bearing (ADR-0018, ADR-0021): declared relaxations
/// enter the canonical form the way budgets fields do. Unlike `budgets`, an
/// undeclared acceptance omits the whole table (not an empty one):
/// acceptance arrived after contract ids were already being cited, and an
/// id must change only when the normative claims do — every pre-acceptance
/// contract keeps its id. `differential_match` follows the same rule at key
/// level: only `"judged"` is emitted (`Exact` is the default posture, not a
/// distinct claim — declaring it changes nothing, so it changes no id),
/// while a judged differential IS a different reproduction claim and must
/// never masquerade under the exact contract's id.
fn acceptance_json(acceptance: &Acceptance) -> Option<Value> {
    let mut m = Map::new();
    if acceptance.differential_match == DifferentialMatch::Judged {
        m.insert("differential_match".to_string(), Value::from("judged"));
    }
    if let Some(milli) = acceptance.differential_min_agreement_milli {
        m.insert(
            "differential_min_agreement_milli".to_string(),
            Value::from(milli),
        );
    }
    if m.is_empty() {
        None
    } else {
        Some(Value::Object(m))
    }
}

fn budgets_json(budgets: &Budgets) -> Value {
    let mut m = Map::new();
    if let Some(v) = budgets.max_cost_usd_micros {
        m.insert("max_cost_usd_micros".to_string(), Value::from(v));
    }
    if let Some(v) = budgets.max_latency_ms_p95 {
        m.insert("max_latency_ms_p95".to_string(), Value::from(v));
    }
    if let Some(v) = budgets.max_tokens {
        m.insert("max_tokens".to_string(), Value::from(v));
    }
    Value::Object(m)
}

fn scope_json(scope: &Scope) -> Value {
    let mut m = Map::new();
    match scope {
        Scope::Task => {
            m.insert("type".to_string(), Value::from("task"));
        }
        Scope::Span { kind, name } => {
            m.insert("kind".to_string(), Value::from(kind.as_str()));
            m.insert("name".to_string(), Value::from(name.as_str()));
            m.insert("type".to_string(), Value::from("span"));
        }
        Scope::Region { from, to } => {
            m.insert("from".to_string(), Value::from(from.as_str()));
            m.insert("to".to_string(), Value::from(to.as_str()));
            m.insert("type".to_string(), Value::from("region"));
        }
    }
    Value::Object(m)
}

fn interface_json(interface: &Interface) -> Value {
    let mut m = Map::new();
    m.insert(
        "input".to_string(),
        Value::String(interface.input.to_string()),
    );
    m.insert(
        "output".to_string(),
        Value::String(interface.output.to_string()),
    );
    Value::Object(m)
}

fn example_json(example: &Example) -> Value {
    let mut m = Map::new();
    m.insert("input".to_string(), example.input.clone());
    // `match` is part of the canonical example form, so the match mode is
    // contract-id-bearing: exact and judged versions of the same example
    // make different normative claims and get different ids (ADR-0019).
    let mode = match example.match_mode {
        MatchMode::Exact => "exact",
        MatchMode::Judged => "judged",
    };
    m.insert("match".to_string(), Value::from(mode));
    m.insert("name".to_string(), Value::String(example.name.clone()));
    m.insert("output".to_string(), example.output.clone());
    Value::Object(m)
}

fn eval_case_json(case: &EvalCase) -> Value {
    let mut m = Map::new();
    if let Some(expected) = &case.expected {
        m.insert("expected".to_string(), expected.clone());
    }
    m.insert("input".to_string(), case.input.clone());
    Value::Object(m)
}

fn property_json(property: &Property) -> Value {
    let mut m = Map::new();
    match property {
        Property::LenRange { target, min, max } => {
            m.insert("kind".to_string(), Value::from("len_range"));
            if let Some(v) = max {
                m.insert("max".to_string(), Value::from(*v));
            }
            if let Some(v) = min {
                m.insert("min".to_string(), Value::from(*v));
            }
            m.insert("target".to_string(), Value::String(target.to_string()));
        }
        Property::Regex { target, pattern } => {
            m.insert("kind".to_string(), Value::from("regex"));
            m.insert("pattern".to_string(), Value::String(pattern.clone()));
            m.insert("target".to_string(), Value::String(target.to_string()));
        }
        Property::NumRange { target, min, max } => {
            m.insert("kind".to_string(), Value::from("num_range"));
            if let Some(v) = max {
                m.insert("max".to_string(), finite_number(*v));
            }
            if let Some(v) = min {
                m.insert("min".to_string(), finite_number(*v));
            }
            m.insert("target".to_string(), Value::String(target.to_string()));
        }
        Property::JsonHasKeys { target, keys } => {
            m.insert(
                "keys".to_string(),
                Value::Array(keys.iter().map(|k| Value::from(k.as_str())).collect()),
            );
            m.insert("kind".to_string(), Value::from("json_has_keys"));
            m.insert("target".to_string(), Value::String(target.to_string()));
        }
        Property::OneOf { target, values } => {
            m.insert("kind".to_string(), Value::from("one_of"));
            m.insert("target".to_string(), Value::String(target.to_string()));
            m.insert("values".to_string(), Value::Array(values.clone()));
        }
    }
    Value::Object(m)
}

/// `NumRange` bounds are validated finite at parse; a hand-built contract
/// with a non-finite bound has no canonical JSON form.
fn finite_number(f: f64) -> Value {
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .expect("NumRange bounds are finite")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn minimal_with(top_level: &str, sections: &str) -> String {
        format!(
            "contract_version = 0\ntask = \"toy-agent\"\n{top_level}\n[scope]\ntype = \"task\"\n\n[interface]\ninput = \"json\"\noutput = \"text\"\n\n{sections}"
        )
    }

    fn with_scope(scope_block: &str) -> String {
        format!(
            "contract_version = 0\ntask = \"t\"\n\n{scope_block}\n\n[interface]\ninput = \"json\"\noutput = \"text\"\n"
        )
    }

    fn example_block(name: &str, mode: &str) -> String {
        format!("[[example]]\nname = \"{name}\"\nmatch = \"{mode}\"\ninput = 1\noutput = 2\n")
    }

    fn property_block(body: &str) -> String {
        format!("[[property]]\n{body}\n")
    }

    fn parse_err(text: &str) -> ContractError {
        from_toml_str(text, Path::new(".")).expect_err("expected rejection")
    }

    #[test]
    fn full_contract_loads_with_eval_sets() {
        let dir = tempfile::tempdir().expect("tempdir");
        // CRLF endings and a blank line: both must be tolerated.
        std::fs::write(
            dir.path().join("cases_a.jsonl"),
            "{\"input\": {\"q\": 1}, \"expected\": [[1], [2]]}\r\n\r\n{\"input\": \"solo\"}\r\n",
        )
        .expect("write cases_a");
        std::fs::write(
            dir.path().join("cases_b.jsonl"),
            "{\"expected\": null, \"input\": 3}\n",
        )
        .expect("write cases_b");
        let text = r#"
contract_version = 0
task = "toy-agent"
eval_sets = ["cases_a.jsonl", "cases_b.jsonl"]

[scope]
type = "span"
kind = "model_call"
name = "classify"

[interface]
input = "json"
output = "list<list<int>>"

[budgets]
max_latency_ms_p95 = 2000
max_cost_usd_micros = 500
max_tokens = 4096

[acceptance]
differential_min_agreement_milli = 900

[[example]]
name = "basic"
match = "exact"
input = { q = "cluster", nums = [3, 1], deep = { flag = true, w = 1.5 } }
output = [[1, 2], [3]]

[[example]]
name = "empty"
match = "exact"
input = { q = "none" }
output = []

[[property]]
kind = "len_range"
target = "output"
min = 1
max = 100

[[property]]
kind = "regex"
target = "output"
pattern = "^[a-z]+$"

[[property]]
kind = "num_range"
target = "output"
min = -1.5
max = 100

[[property]]
kind = "json_has_keys"
target = "output"
keys = ["a", "b"]

[[property]]
kind = "one_of"
target = "output"
values = [[[1]], []]
"#;
        let contract_path = dir.path().join("toy.toml");
        std::fs::write(&contract_path, text).expect("write contract");

        let c = load(&contract_path).expect("contract loads");
        assert_eq!(c.task, "toy-agent");
        assert_eq!(
            c.scope,
            Scope::Span {
                kind: "model_call".to_string(),
                name: "classify".to_string(),
            }
        );
        assert_eq!(c.interface.input, ValueType::Json);
        assert_eq!(
            c.interface.output,
            ValueType::List(Box::new(ValueType::List(Box::new(ValueType::Int))))
        );
        assert_eq!(c.examples.len(), 2);
        assert_eq!(c.examples[0].name, "basic");
        assert_eq!(c.examples[0].match_mode, MatchMode::Exact);
        assert_eq!(
            c.examples[0].input,
            serde_json::json!({"q": "cluster", "nums": [3, 1], "deep": {"flag": true, "w": 1.5}})
        );
        assert_eq!(c.examples[0].output, serde_json::json!([[1, 2], [3]]));
        assert_eq!(c.examples[1].output, serde_json::json!([]));
        assert_eq!(
            c.properties,
            vec![
                Property::LenRange {
                    target: Target::Output,
                    min: Some(1),
                    max: Some(100),
                },
                Property::Regex {
                    target: Target::Output,
                    pattern: "^[a-z]+$".to_string(),
                },
                Property::NumRange {
                    target: Target::Output,
                    min: Some(-1.5),
                    max: Some(100.0),
                },
                Property::JsonHasKeys {
                    target: Target::Output,
                    keys: vec!["a".to_string(), "b".to_string()],
                },
                Property::OneOf {
                    target: Target::Output,
                    values: vec![serde_json::json!([[1]]), serde_json::json!([])],
                },
            ]
        );
        assert_eq!(
            c.budgets,
            Budgets {
                max_latency_ms_p95: Some(2000),
                max_cost_usd_micros: Some(500),
                max_tokens: Some(4096),
            }
        );
        assert_eq!(
            c.acceptance,
            Acceptance {
                differential_min_agreement_milli: Some(900),
                differential_match: DifferentialMatch::Exact,
            }
        );
        // declared file order, then line order; blank lines skipped
        assert_eq!(
            c.eval_cases,
            vec![
                EvalCase {
                    input: serde_json::json!({"q": 1}),
                    expected: Some(serde_json::json!([[1], [2]])),
                },
                EvalCase {
                    input: serde_json::json!("solo"),
                    expected: None,
                },
                EvalCase {
                    input: serde_json::json!(3),
                    expected: Some(Value::Null),
                },
            ]
        );

        let again = from_toml_str(text, dir.path()).expect("reparse");
        assert_eq!(c, again);
        assert_eq!(c.id(), again.id());
        assert_eq!(c.id().0.len(), 64);
    }

    #[test]
    fn canonical_json_shape_is_pinned() {
        let c = from_toml_str(&minimal_with("", ""), Path::new(".")).expect("parse");
        // no `acceptance` key: an undeclared acceptance is omitted from the
        // canonical form, so every pre-acceptance contract id is unchanged
        // (ADR-0018) — this pinned string predates the field.
        assert_eq!(
            c.canonical_json(),
            r#"{"budgets":{},"contract_version":0,"eval_cases":[],"examples":[],"interface":{"input":"json","output":"text"},"properties":[],"scope":{"type":"task"},"task":"toy-agent"}"#
        );
        assert_eq!(c.budgets, Budgets::default());
        assert_eq!(c.acceptance, Acceptance::default());
        assert_eq!(c.id().0, auto_trace::model::digest_hex(&c.canonical_json()));
    }

    #[test]
    fn canonical_json_with_acceptance_is_pinned() {
        let c = from_toml_str(
            &minimal_with("", "[acceptance]\ndifferential_min_agreement_milli = 650\n"),
            Path::new("."),
        )
        .expect("parse");
        assert_eq!(
            c.canonical_json(),
            r#"{"acceptance":{"differential_min_agreement_milli":650},"budgets":{},"contract_version":0,"eval_cases":[],"examples":[],"interface":{"input":"json","output":"text"},"properties":[],"scope":{"type":"task"},"task":"toy-agent"}"#
        );
    }

    #[test]
    fn canonical_json_omits_absent_optionals() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("e.jsonl"),
            "{\"input\": 1}\n{\"input\": 2, \"expected\": null}\n",
        )
        .expect("write eval set");
        let text = minimal_with(
            "eval_sets = [\"e.jsonl\"]\n",
            "[budgets]\nmax_tokens = 9\n\n[[property]]\nkind = \"len_range\"\ntarget = \"output\"\nmin = 2\n",
        );
        let c = from_toml_str(&text, dir.path()).expect("parse");
        assert_eq!(c.eval_cases[0].expected, None);
        assert_eq!(c.eval_cases[1].expected, Some(Value::Null));
        assert_eq!(
            c.canonical_json(),
            r#"{"budgets":{"max_tokens":9},"contract_version":0,"eval_cases":[{"input":1},{"expected":null,"input":2}],"examples":[],"interface":{"input":"json","output":"text"},"properties":[{"kind":"len_range","min":2,"target":"output"}],"scope":{"type":"task"},"task":"toy-agent"}"#
        );
    }

    #[test]
    fn canonical_json_with_judged_differential_is_pinned() {
        // the judged differential is id-bearing (ADR-0021): `"judged"` enters
        // the canonical acceptance table; declared `"exact"` does NOT (it is
        // the default posture made explicit, not a distinct claim) — so a
        // declared-exact contract keeps the ADR-0018 canonical form and id.
        let judged = from_toml_str(
            &minimal_with(
                "",
                "[acceptance]\ndifferential_min_agreement_milli = 650\ndifferential_match = \"judged\"\n",
            ),
            Path::new("."),
        )
        .expect("parse judged");
        assert_eq!(
            judged.canonical_json(),
            r#"{"acceptance":{"differential_match":"judged","differential_min_agreement_milli":650},"budgets":{},"contract_version":0,"eval_cases":[],"examples":[],"interface":{"input":"json","output":"text"},"properties":[],"scope":{"type":"task"},"task":"toy-agent"}"#
        );
        let declared_exact = from_toml_str(
            &minimal_with(
                "",
                "[acceptance]\ndifferential_min_agreement_milli = 650\ndifferential_match = \"exact\"\n",
            ),
            Path::new("."),
        )
        .expect("parse declared exact");
        assert_eq!(
            declared_exact.canonical_json(),
            r#"{"acceptance":{"differential_min_agreement_milli":650},"budgets":{},"contract_version":0,"eval_cases":[],"examples":[],"interface":{"input":"json","output":"text"},"properties":[],"scope":{"type":"task"},"task":"toy-agent"}"#
        );
        assert_ne!(judged.id(), declared_exact.id());
    }

    #[test]
    fn id_is_stable_and_content_sensitive() {
        let a = from_toml_str(&minimal_with("", ""), Path::new(".")).expect("parse a");
        let b = from_toml_str(&minimal_with("", ""), Path::new(".")).expect("parse b");
        assert_eq!(a.id(), b.id());

        let changed = minimal_with("", "").replace("toy-agent", "other-task");
        let c = from_toml_str(&changed, Path::new(".")).expect("parse changed");
        assert_ne!(a.id(), c.id());
    }

    // -- [acceptance] (ADR-0018) ---------------------------------------------

    fn acceptance_at(milli: &str) -> String {
        minimal_with(
            "",
            &format!("[acceptance]\ndifferential_min_agreement_milli = {milli}\n"),
        )
    }

    #[test]
    fn acceptance_absent_empty_and_declared_round_trip() {
        // absent table = default = exact
        let absent = from_toml_str(&minimal_with("", ""), Path::new(".")).expect("parse");
        assert_eq!(absent.acceptance, Acceptance::default());
        // an empty table declares nothing: default, same id as absent
        let empty =
            from_toml_str(&minimal_with("", "[acceptance]\n"), Path::new(".")).expect("parse");
        assert_eq!(empty.acceptance, Acceptance::default());
        assert_eq!(absent.id(), empty.id());
        // declared values round-trip, including both range ends
        for milli in [1u32, 666, 1000] {
            let c = from_toml_str(&acceptance_at(&milli.to_string()), Path::new("."))
                .expect("declared acceptance parses");
            assert_eq!(c.acceptance.differential_min_agreement_milli, Some(milli));
            let again =
                from_toml_str(&acceptance_at(&milli.to_string()), Path::new(".")).expect("reparse");
            assert_eq!(c, again);
            assert_eq!(c.id(), again.id());
        }
    }

    #[test]
    fn acceptance_out_of_range_rejected() {
        // 0 declares "no agreement required" — a vacuous gate, rejected
        let err = parse_err(&acceptance_at("0"));
        assert!(matches!(err, ContractError::Toml { message } if message.contains("1..=1000")));
        // above 1000 is not a rate
        let err = parse_err(&acceptance_at("1001"));
        assert!(matches!(err, ContractError::Toml { message } if message.contains("1..=1000")));
        // negative fails the unsigned read
        let err = parse_err(&acceptance_at("-1"));
        assert!(matches!(err, ContractError::Toml { message } if message.contains("unsigned")));
    }

    #[test]
    fn acceptance_non_integer_rejected() {
        let err = parse_err(&acceptance_at("0.9"));
        assert!(matches!(err, ContractError::Toml { message } if message.contains("integer")));
        let err = parse_err(&acceptance_at("\"all\""));
        assert!(matches!(err, ContractError::Toml { message } if message.contains("integer")));
    }

    #[test]
    fn acceptance_unknown_key_rejected() {
        let err = parse_err(&minimal_with("", "[acceptance]\ntolerance = 3\n"));
        assert!(matches!(err, ContractError::Toml { message } if message.contains("tolerance")));
    }

    #[test]
    fn acceptance_is_id_bearing() {
        let parse_at = |milli: &str| {
            from_toml_str(&acceptance_at(milli), Path::new(".")).expect("acceptance parses")
        };
        let base = from_toml_str(&minimal_with("", ""), Path::new(".")).expect("parse");
        // stable when equal
        assert_eq!(parse_at("800").id(), parse_at("800").id());
        // different acceptance = different normative claim = different id;
        // declared-exact (1000) differs from undeclared-exact
        assert_ne!(base.id(), parse_at("800").id());
        assert_ne!(parse_at("800").id(), parse_at("801").id());
        assert_ne!(base.id(), parse_at("1000").id());
    }

    #[test]
    fn value_type_round_trips_display() {
        for text in [
            "unit",
            "bool",
            "int",
            "float",
            "text",
            "bytes",
            "json",
            "list<text>",
            "list<list<int>>",
            "list<list<list<json>>>",
        ] {
            let parsed = parse_value_type(text).expect("parses");
            assert_eq!(parsed.to_string(), text);
        }
        assert_eq!(
            parse_value_type("list<list<int>>"),
            Some(ValueType::List(Box::new(ValueType::List(Box::new(
                ValueType::Int
            )))))
        );
    }

    #[test]
    fn value_type_rejects_off_grammar_text() {
        for text in [
            "",
            "Int",
            "TEXT",
            "string",
            "list",
            "list<",
            "list<>",
            "list< int >",
            "list<int> ",
            " int",
            "int ",
            "list<int>>",
            "list<list<int>",
        ] {
            assert_eq!(parse_value_type(text), None, "{text:?} must not parse");
        }
    }

    proptest! {
        #[test]
        fn value_type_display_round_trips_any(vt in value_type_strategy()) {
            let text = vt.to_string();
            prop_assert_eq!(parse_value_type(&text), Some(vt));
        }
    }

    fn value_type_strategy() -> impl Strategy<Value = ValueType> {
        let leaf = prop_oneof![
            Just(ValueType::Unit),
            Just(ValueType::Bool),
            Just(ValueType::Int),
            Just(ValueType::Float),
            Just(ValueType::Text),
            Just(ValueType::Bytes),
            Just(ValueType::Json),
        ];
        leaf.prop_recursive(4, 16, 1, |inner| {
            inner.prop_map(|t| ValueType::List(Box::new(t)))
        })
    }

    #[test]
    fn io_error_on_missing_contract_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = load(&dir.path().join("absent.toml")).expect_err("missing file");
        assert!(matches!(err, ContractError::Io(_)));
    }

    #[test]
    fn toml_rejections() {
        // syntax error
        assert!(matches!(
            parse_err("not == toml"),
            ContractError::Toml { .. }
        ));
        // unknown top-level key
        let err = parse_err(&minimal_with("mystery = 1\n", ""));
        assert!(matches!(err, ContractError::Toml { message } if message.contains("mystery")));
        // missing required top-level key
        let err = parse_err("contract_version = 0\ntask = \"t\"\n[scope]\ntype = \"task\"\n");
        assert!(matches!(err, ContractError::Toml { message } if message.contains("interface")));
        // unknown key nested in a table
        let err = parse_err(&with_scope("[scope]\ntype = \"task\"\nnote = \"x\""));
        assert!(matches!(err, ContractError::Toml { message } if message.contains("note")));
        let err = parse_err(&minimal_with("", "[budgets]\nmax_rage = 3\n"));
        assert!(matches!(err, ContractError::Toml { message } if message.contains("max_rage")));
        // budgets must be non-negative integers
        let err = parse_err(&minimal_with("", "[budgets]\nmax_tokens = -1\n"));
        assert!(matches!(err, ContractError::Toml { message } if message.contains("non-negative")));
    }

    #[test]
    fn unsupported_version_rejected() {
        let text = minimal_with("", "").replace("contract_version = 0", "contract_version = 3");
        assert!(matches!(
            parse_err(&text),
            ContractError::UnsupportedVersion { found: 3 }
        ));
    }

    #[test]
    fn empty_task_rejected() {
        let text = minimal_with("", "").replace("task = \"toy-agent\"", "task = \"\"");
        assert!(matches!(
            parse_err(&text),
            ContractError::EmptyField { field: "task" }
        ));
    }

    #[test]
    fn unknown_scope_type_rejected() {
        let err = parse_err(&with_scope("[scope]\ntype = \"trace\""));
        assert!(matches!(err, ContractError::UnknownScopeType(t) if t == "trace"));
    }

    #[test]
    fn unknown_span_kind_rejected() {
        let err = parse_err(&with_scope(
            "[scope]\ntype = \"span\"\nkind = \"http_call\"\nname = \"get\"",
        ));
        assert!(matches!(err, ContractError::UnknownSpanKind(k) if k == "http_call"));
    }

    #[test]
    fn scope_field_mismatch_rejected() {
        // task scope must not carry span fields
        let err = parse_err(&with_scope(
            "[scope]\ntype = \"task\"\nkind = \"tool_call\"",
        ));
        assert!(matches!(err, ContractError::ScopeFieldMismatch { .. }));
        // span scope requires kind and name
        let err = parse_err(&with_scope(
            "[scope]\ntype = \"span\"\nkind = \"tool_call\"",
        ));
        assert!(matches!(err, ContractError::ScopeFieldMismatch { .. }));
    }

    #[test]
    fn empty_span_name_rejected() {
        let err = parse_err(&with_scope(
            "[scope]\ntype = \"span\"\nkind = \"tool_call\"\nname = \"\"",
        ));
        assert!(matches!(
            err,
            ContractError::EmptyField {
                field: "scope.name"
            }
        ));
    }

    #[test]
    fn span_scope_parses_every_kind() {
        for kind in SPAN_KINDS {
            let text = with_scope(&format!(
                "[scope]\ntype = \"span\"\nkind = \"{kind}\"\nname = \"op\""
            ));
            let c = from_toml_str(&text, Path::new(".")).expect("span scope parses");
            assert_eq!(
                c.scope,
                Scope::Span {
                    kind: kind.to_string(),
                    name: "op".to_string(),
                }
            );
        }
    }

    #[test]
    fn bad_value_type_rejected() {
        let src = minimal_with("", "").replace("input = \"json\"", "input = \"list<\"");
        let err = parse_err(&src);
        assert!(matches!(
            err,
            ContractError::BadValueType { field, text } if field == "interface.input" && text == "list<"
        ));
        let src = minimal_with("", "").replace("output = \"text\"", "output = \"Text\"");
        let err = parse_err(&src);
        assert!(matches!(
            err,
            ContractError::BadValueType { field, .. } if field == "interface.output"
        ));
    }

    #[test]
    fn unknown_match_mode_rejected() {
        // "judged" is a real mode since ADR-0019; anything else still rejects
        let err = parse_err(&minimal_with("", &example_block("a", "semantic")));
        assert!(matches!(err, ContractError::UnknownMatchMode(m) if m == "semantic"));
        let err = parse_err(&minimal_with("", &example_block("a", "Judged")));
        assert!(matches!(err, ContractError::UnknownMatchMode(m) if m == "Judged"));
    }

    #[test]
    fn judged_match_mode_parses_and_round_trips() {
        let text = minimal_with("", &example_block("a", "judged"));
        let c = from_toml_str(&text, Path::new(".")).expect("judged example parses");
        assert_eq!(c.examples.len(), 1);
        assert_eq!(c.examples[0].match_mode, MatchMode::Judged);
        let again = from_toml_str(&text, Path::new(".")).expect("reparse");
        assert_eq!(c, again);
        assert_eq!(c.id(), again.id());
        // the mode is in the canonical example form (the id-bearing preimage)
        assert!(
            c.canonical_json().contains(r#""match":"judged""#),
            "{}",
            c.canonical_json()
        );
    }

    #[test]
    fn match_mode_is_id_bearing() {
        // exact vs judged versions of the SAME example are different
        // normative claims and get different ids (ADR-0019)
        let exact = from_toml_str(
            &minimal_with("", &example_block("a", "exact")),
            Path::new("."),
        )
        .expect("exact parses");
        let judged = from_toml_str(
            &minimal_with("", &example_block("a", "judged")),
            Path::new("."),
        )
        .expect("judged parses");
        assert_ne!(exact.id(), judged.id());
    }

    #[test]
    fn duplicate_example_name_rejected() {
        let sections = format!(
            "{}{}",
            example_block("a", "exact"),
            example_block("a", "exact")
        );
        let err = parse_err(&minimal_with("", &sections));
        assert!(matches!(err, ContractError::DuplicateExampleName(n) if n == "a"));
    }

    #[test]
    fn empty_example_name_rejected() {
        let err = parse_err(&minimal_with("", &example_block("", "exact")));
        assert!(matches!(
            err,
            ContractError::EmptyField {
                field: "example.name"
            }
        ));
    }

    #[test]
    fn unknown_target_rejected() {
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"len_range\"\ntarget = \"input\"\nmin = 1"),
        ));
        assert!(matches!(err, ContractError::UnknownTarget(t) if t == "input"));
    }

    #[test]
    fn unknown_property_kind_rejected() {
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"starts_with\"\ntarget = \"output\""),
        ));
        assert!(matches!(err, ContractError::UnknownPropertyKind(k) if k == "starts_with"));
    }

    #[test]
    fn unknown_property_field_rejected() {
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"len_range\"\ntarget = \"output\"\nmin = 1\npattern = \"x\""),
        ));
        assert!(matches!(
            err,
            ContractError::UnknownPropertyField { kind, field }
                if kind == "len_range" && field == "pattern"
        ));
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"regex\"\ntarget = \"output\"\npattern = \"x\"\nmax = 1"),
        ));
        assert!(matches!(
            err,
            ContractError::UnknownPropertyField { kind, field }
                if kind == "regex" && field == "max"
        ));
    }

    #[test]
    fn invalid_range_rejected() {
        // no bounds at all
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"len_range\"\ntarget = \"output\""),
        ));
        assert!(matches!(err, ContractError::InvalidRange { .. }));
        // min > max
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"len_range\"\ntarget = \"output\"\nmin = 5\nmax = 2"),
        ));
        assert!(matches!(err, ContractError::InvalidRange { .. }));
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"num_range\"\ntarget = \"output\"\nmin = 2.5\nmax = 1.0"),
        ));
        assert!(matches!(err, ContractError::InvalidRange { .. }));
    }

    #[test]
    fn bad_regex_rejected() {
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"regex\"\ntarget = \"output\"\npattern = \"(\""),
        ));
        assert!(matches!(err, ContractError::BadRegex { pattern, .. } if pattern == "("));
    }

    #[test]
    fn empty_property_arrays_rejected() {
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"json_has_keys\"\ntarget = \"output\"\nkeys = []"),
        ));
        assert!(matches!(
            err,
            ContractError::EmptyField {
                field: "property.json_has_keys.keys"
            }
        ));
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"json_has_keys\"\ntarget = \"output\"\nkeys = [\"a\", \"\"]"),
        ));
        assert!(matches!(
            err,
            ContractError::EmptyField {
                field: "property.json_has_keys.keys"
            }
        ));
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"one_of\"\ntarget = \"output\"\nvalues = []"),
        ));
        assert!(matches!(
            err,
            ContractError::EmptyField {
                field: "property.one_of.values"
            }
        ));
    }

    #[test]
    fn datetime_rejected_everywhere() {
        let sections = "[[example]]\nname = \"a\"\nmatch = \"exact\"\ninput = 1979-05-27T07:32:00Z\noutput = 1\n";
        let err = parse_err(&minimal_with("", sections));
        assert!(matches!(
            err,
            ContractError::DatetimeNotSupported { location } if location == "example[0].input"
        ));
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"one_of\"\ntarget = \"output\"\nvalues = [1979-05-27]"),
        ));
        assert!(matches!(err, ContractError::DatetimeNotSupported { .. }));
    }

    #[test]
    fn non_finite_numbers_rejected() {
        let sections = "[[example]]\nname = \"a\"\nmatch = \"exact\"\ninput = 1\noutput = inf\n";
        let err = parse_err(&minimal_with("", sections));
        assert!(matches!(
            err,
            ContractError::NonFiniteNumber { location } if location == "example[0].output"
        ));
        let err = parse_err(&minimal_with(
            "",
            &property_block("kind = \"num_range\"\ntarget = \"output\"\nmin = nan"),
        ));
        assert!(matches!(err, ContractError::NonFiniteNumber { .. }));
    }

    #[test]
    fn eval_set_io_error_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = minimal_with("eval_sets = [\"missing.jsonl\"]\n", "");
        let err = from_toml_str(&src, dir.path()).expect_err("missing eval set");
        assert!(matches!(
            err,
            ContractError::EvalSetIo { path, .. } if path == dir.path().join("missing.jsonl")
        ));
    }

    #[test]
    fn bad_eval_set_lines_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = minimal_with("eval_sets = [\"cases.jsonl\"]\n", "");
        let cases = dir.path().join("cases.jsonl");

        let check = |content: &str, line: usize, needle: &str| {
            std::fs::write(&cases, content).expect("write eval set");
            let err = from_toml_str(&src, dir.path()).expect_err("bad eval set");
            match err {
                ContractError::BadEvalSet { line: l, why, .. } => {
                    assert_eq!(l, line, "line number for {content:?}");
                    assert!(why.contains(needle), "why={why:?} for {content:?}");
                }
                other => panic!("expected BadEvalSet, got {other:?}"),
            }
        };

        check("{\"input\": 1, \"extra\": 2}\n", 1, "unknown field `extra`");
        check(
            "\n\n{\"expected\": 2}\n",
            3,
            "missing required field `input`",
        );
        check("[1, 2]\n", 1, "not a json object");
        check("{\"input\": \n", 1, "invalid json");
    }
}
