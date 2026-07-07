//! Value ↔ type conformance: does a JSON value inhabit an IR [`ValueType`]?
//!
//! Rules (spec/contract.md):
//!
//! - `unit` — JSON `null` only.
//! - `bool` — JSON booleans.
//! - `int` — JSON numbers representable as `i64`. Unsigned values beyond
//!   `i64::MAX` are rejected (found `number beyond i64`); float-typed
//!   numbers are rejected even when whole — `3.0` is a float, not an int
//!   (found `non-integer number`).
//! - `float` — any JSON number; the integer-typed `3` conforms.
//! - `text` — JSON strings.
//! - `bytes` — JSON strings, treated as opaque; the encoding is the
//!   producer's concern (spec/contract.md).
//! - `json` — anything.
//! - `list<T>` — JSON arrays whose every element conforms to `T`.

use auto_ir::ValueType;
use serde_json::Value;

/// Conformance failure at one location.
///
/// `path` walks list indices from the root: `$`, `$[2]`, `$[1][0]`.
/// `expected` is the [`ValueType`] display form (e.g. `list<text>`).
/// `found` is a short JSON type name (`null`/`bool`/`number`/`string`/
/// `array`/`object`) or a precision note (`non-integer number`,
/// `number beyond i64`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("at {path}: expected {expected}, found {found}")]
pub struct ConformError {
    pub path: String,
    pub expected: String,
    pub found: String,
}

/// Check `value` against `ty` under the module rules. On mismatch the error
/// locates the first offending element in document order.
pub fn conforms(value: &Value, ty: &ValueType) -> Result<(), ConformError> {
    check_at(value, ty, &mut Vec::new())
}

/// Short JSON type name used in `found` fields and property-failure details.
pub(crate) fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Recursive worker. `indices` holds the list indices from the root down to
/// `value`; the path string is materialized only on failure.
fn check_at(value: &Value, ty: &ValueType, indices: &mut Vec<usize>) -> Result<(), ConformError> {
    match ty {
        ValueType::Unit => match value {
            Value::Null => Ok(()),
            other => Err(mismatch(indices, ty, json_type_name(other))),
        },
        ValueType::Bool => match value {
            Value::Bool(_) => Ok(()),
            other => Err(mismatch(indices, ty, json_type_name(other))),
        },
        ValueType::Int => match value {
            Value::Number(n) if n.as_i64().is_some() => Ok(()),
            // a number that is not i64-representable is either an unsigned
            // value beyond i64::MAX or float-typed (3.0, 3.5)
            Value::Number(n) if n.is_u64() => Err(mismatch(indices, ty, "number beyond i64")),
            Value::Number(_) => Err(mismatch(indices, ty, "non-integer number")),
            other => Err(mismatch(indices, ty, json_type_name(other))),
        },
        ValueType::Float => match value {
            Value::Number(_) => Ok(()),
            other => Err(mismatch(indices, ty, json_type_name(other))),
        },
        ValueType::Text | ValueType::Bytes => match value {
            Value::String(_) => Ok(()),
            other => Err(mismatch(indices, ty, json_type_name(other))),
        },
        ValueType::Json => Ok(()),
        ValueType::List(elem) => match value {
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    indices.push(i);
                    check_at(item, elem, indices)?;
                    indices.pop();
                }
                Ok(())
            }
            other => Err(mismatch(indices, ty, json_type_name(other))),
        },
    }
}

fn mismatch(indices: &[usize], expected: &ValueType, found: &str) -> ConformError {
    let mut path = String::from("$");
    for i in indices {
        path.push_str(&format!("[{i}]"));
    }
    ConformError {
        path,
        expected: expected.to_string(),
        found: found.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use auto_ir::ValueType;
    use serde_json::{Value, json};

    use super::{ConformError, conforms};

    fn list(elem: ValueType) -> ValueType {
        ValueType::List(Box::new(elem))
    }

    fn err(value: &Value, ty: &ValueType) -> ConformError {
        conforms(value, ty).expect_err("expected a conformance failure")
    }

    /// Full accept/reject matrix: every scalar type against every JSON shape.
    #[test]
    fn scalar_matrix() {
        let samples = [
            ("null", json!(null)),
            ("bool", json!(true)),
            ("int number", json!(3)),
            ("float number", json!(3.0)),
            ("big u64", json!(u64::MAX)),
            ("string", json!("s")),
            ("array", json!([1])),
            ("object", json!({"k": 1})),
        ];
        let all = [
            "null",
            "bool",
            "int number",
            "float number",
            "big u64",
            "string",
            "array",
            "object",
        ];
        let matrix: &[(ValueType, &[&str])] = &[
            (ValueType::Unit, &["null"]),
            (ValueType::Bool, &["bool"]),
            (ValueType::Int, &["int number"]),
            (ValueType::Float, &["int number", "float number", "big u64"]),
            (ValueType::Text, &["string"]),
            (ValueType::Bytes, &["string"]),
            (ValueType::Json, &all),
        ];
        for (ty, accepted) in matrix {
            for (label, value) in &samples {
                assert_eq!(
                    conforms(value, ty).is_ok(),
                    accepted.contains(label),
                    "{label} vs {ty}"
                );
            }
        }
    }

    #[test]
    fn int_accepts_full_i64_range() {
        assert_eq!(conforms(&json!(i64::MAX), &ValueType::Int), Ok(()));
        assert_eq!(conforms(&json!(i64::MIN), &ValueType::Int), Ok(()));
        assert_eq!(conforms(&json!(0), &ValueType::Int), Ok(()));
    }

    #[test]
    fn int_rejects_u64_beyond_i64() {
        let just_over = json!((i64::MAX as u64) + 1);
        let e = err(&just_over, &ValueType::Int);
        assert_eq!(e.path, "$");
        assert_eq!(e.expected, "int");
        assert_eq!(e.found, "number beyond i64");
        assert_eq!(
            err(&json!(u64::MAX), &ValueType::Int).found,
            "number beyond i64"
        );
    }

    #[test]
    fn int_float_distinction() {
        // 3 is an int; 3.0 is float-typed and NOT an int
        assert_eq!(conforms(&json!(3), &ValueType::Int), Ok(()));
        assert_eq!(
            err(&json!(3.0), &ValueType::Int).found,
            "non-integer number"
        );
        assert_eq!(
            err(&json!(3.5), &ValueType::Int).found,
            "non-integer number"
        );
        // float accepts all of them, including the integer-typed 3
        assert_eq!(conforms(&json!(3), &ValueType::Float), Ok(()));
        assert_eq!(conforms(&json!(3.0), &ValueType::Float), Ok(()));
        assert_eq!(conforms(&json!(3.5), &ValueType::Float), Ok(()));
    }

    #[test]
    fn int_rejects_non_numbers_with_type_name() {
        let e = err(&json!("3"), &ValueType::Int);
        assert_eq!(e.found, "string");
        assert_eq!(e.to_string(), "at $: expected int, found string");
    }

    #[test]
    fn unit_and_bool_error_contents() {
        let e = err(&json!(false), &ValueType::Unit);
        assert_eq!((e.expected.as_str(), e.found.as_str()), ("unit", "bool"));
        let e = err(&json!(null), &ValueType::Bool);
        assert_eq!((e.expected.as_str(), e.found.as_str()), ("bool", "null"));
    }

    #[test]
    fn bytes_is_a_string_shape() {
        assert_eq!(conforms(&json!("AAEC"), &ValueType::Bytes), Ok(()));
        let e = err(&json!([0, 1, 2]), &ValueType::Bytes);
        assert_eq!((e.expected.as_str(), e.found.as_str()), ("bytes", "array"));
    }

    #[test]
    fn everything_conforms_to_json() {
        let deep = json!({"a": [1, "x", null, {"b": [true]}]});
        assert_eq!(conforms(&deep, &ValueType::Json), Ok(()));
    }

    #[test]
    fn list_accepts_conforming_arrays() {
        assert_eq!(conforms(&json!([]), &list(ValueType::Int)), Ok(()));
        assert_eq!(conforms(&json!([]), &list(list(ValueType::Text))), Ok(()));
        assert_eq!(conforms(&json!([1, 2, 3]), &list(ValueType::Int)), Ok(()));
        // list<json> is heterogeneous by construction
        assert_eq!(
            conforms(&json!([1, "a", null]), &list(ValueType::Json)),
            Ok(())
        );
    }

    #[test]
    fn list_top_level_mismatch_shows_display_type() {
        let e = err(&json!("x"), &list(ValueType::Text));
        assert_eq!(e.path, "$");
        assert_eq!(e.expected, "list<text>");
        assert_eq!(e.found, "string");
        let e = err(&json!(3), &list(list(ValueType::Int)));
        assert_eq!(e.expected, "list<list<int>>");
    }

    #[test]
    fn list_element_error_path() {
        let e = err(&json!([1, 2, "x"]), &list(ValueType::Int));
        assert_eq!(e.path, "$[2]");
        assert_eq!(e.expected, "int");
        assert_eq!(e.found, "string");
    }

    #[test]
    fn nested_list_error_path() {
        let e = err(&json!([[1], [true]]), &list(list(ValueType::Int)));
        assert_eq!(e.path, "$[1][0]");
        assert_eq!(e.expected, "int");
        assert_eq!(e.found, "bool");
        assert_eq!(e.to_string(), "at $[1][0]: expected int, found bool");
    }

    #[test]
    fn error_path_after_passing_siblings() {
        // earlier passing elements must not distort the failure path
        let e = err(&json!([[1, 2], [3, "x"]]), &list(list(ValueType::Int)));
        assert_eq!(e.path, "$[1][1]");
        assert_eq!(e.found, "string");
    }
}
