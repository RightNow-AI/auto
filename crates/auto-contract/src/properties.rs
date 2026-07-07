//! Property checking engine: the closed v0 predicate set (spec/contract.md).
//!
//! [`check`] evaluates one [`Property`] against one JSON value. It is total:
//! it never panics, and a property applied to a value of the wrong shape
//! **fails** — it does not skip. `detail` is `Some` on every failure, `None`
//! on pass.
//!
//! Outcome `description` strings are deterministic and terse:
//!
//! - `len_range(output, 1..=200)` — an open side drops its number
//!   (`..=200`, `1..=`)
//! - `regex(output, "PATTERN")` — pattern verbatim
//! - `num_range(output, 0..=100)` — bounds formatted with `{}` (`0.5`, not
//!   `0.50`; `100`, not `100.0`)
//! - `json_has_keys(output, ["a", "b"])`
//! - `one_of(output, 3 values)` — candidate count, never the candidates
//!
//! Semantics:
//!
//! - **LenRange** — string length is `chars().count()` (unicode chars, not
//!   bytes); array length is the element count. Bounds inclusive. Any other
//!   shape fails with detail `len_range does not apply to <type>`.
//! - **Regex** — strings only. Rust `regex` **search** semantics
//!   ([`regex::Regex::is_match`]); authors anchor explicitly for a full
//!   match. Patterns are pre-validated at parse; a compile failure here is
//!   still handled as a failed outcome, never a panic.
//! - **NumRange** — numbers only, compared as `f64` (integers beyond 2^53
//!   lose precision here). Bounds inclusive.
//! - **JsonHasKeys** — JSON objects only; every listed key must be present.
//!   The failure detail lists the missing keys.
//! - **OneOf** — passes iff the value equals any candidate under
//!   canonical-json equality ([`auto_trace::model::canonical_json`] on both
//!   sides), which makes object key order irrelevant.

use std::fmt;

use auto_trace::model::canonical_json;
use serde_json::Value;

use crate::conform::json_type_name;
use crate::model::Property;

/// Result of checking one property against one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyOutcome {
    /// deterministic rendering of the property, e.g. `len_range(output, 1..=200)`
    pub description: String,
    pub passed: bool,
    /// short failure reason; `Some` iff `passed` is false
    pub detail: Option<String>,
}

/// Check one property against a value. Total: never panics; a wrong-shaped
/// value fails, it does not skip.
pub fn check(property: &Property, value: &Value) -> PropertyOutcome {
    let (passed, detail) = match property {
        Property::LenRange { min, max, .. } => check_len(*min, *max, value),
        Property::Regex { pattern, .. } => check_regex(pattern, value),
        Property::NumRange { min, max, .. } => check_num(*min, *max, value),
        Property::JsonHasKeys { keys, .. } => check_keys(keys, value),
        Property::OneOf { values, .. } => check_one_of(values, value),
    };
    PropertyOutcome {
        description: describe(property),
        passed,
        detail,
    }
}

fn describe(property: &Property) -> String {
    match property {
        Property::LenRange { target, min, max } => {
            format!("len_range({target}, {})", fmt_bounds(*min, *max))
        }
        Property::Regex { target, pattern } => format!("regex({target}, \"{pattern}\")"),
        Property::NumRange { target, min, max } => {
            format!("num_range({target}, {})", fmt_bounds(*min, *max))
        }
        Property::JsonHasKeys { target, keys } => format!("json_has_keys({target}, {keys:?})"),
        Property::OneOf { target, values } => {
            format!("one_of({target}, {} values)", values.len())
        }
    }
}

/// `min..=max` with an open side rendered empty: `1..=200`, `..=200`,
/// `1..=`. Floats format with `{}`.
fn fmt_bounds<T: fmt::Display>(min: Option<T>, max: Option<T>) -> String {
    let lo = min.map_or_else(String::new, |v| v.to_string());
    let hi = max.map_or_else(String::new, |v| v.to_string());
    format!("{lo}..={hi}")
}

fn check_len(min: Option<u64>, max: Option<u64>, value: &Value) -> (bool, Option<String>) {
    let len: u64 = match value {
        Value::String(s) => s.chars().count() as u64,
        Value::Array(items) => items.len() as u64,
        other => {
            let ty = json_type_name(other);
            return (false, Some(format!("len_range does not apply to {ty}")));
        }
    };
    if min.is_none_or(|m| len >= m) && max.is_none_or(|m| len <= m) {
        (true, None)
    } else {
        (
            false,
            Some(format!("length {len} outside {}", fmt_bounds(min, max))),
        )
    }
}

fn check_regex(pattern: &str, value: &Value) -> (bool, Option<String>) {
    let Value::String(text) = value else {
        let ty = json_type_name(value);
        return (false, Some(format!("regex does not apply to {ty}")));
    };
    // pre-validated at parse; handled defensively anyway — check is total
    let re = match regex::Regex::new(pattern) {
        Ok(re) => re,
        Err(e) => return (false, Some(format!("pattern failed to compile: {e}"))),
    };
    if re.is_match(text) {
        (true, None)
    } else {
        (false, Some("no match".to_string()))
    }
}

fn check_num(min: Option<f64>, max: Option<f64>, value: &Value) -> (bool, Option<String>) {
    let Value::Number(n) = value else {
        let ty = json_type_name(value);
        return (false, Some(format!("num_range does not apply to {ty}")));
    };
    let Some(x) = n.as_f64() else {
        // unreachable without serde_json's arbitrary_precision; defensive
        return (false, Some("number not representable as f64".to_string()));
    };
    if min.is_none_or(|m| x >= m) && max.is_none_or(|m| x <= m) {
        (true, None)
    } else {
        (
            false,
            Some(format!("value {x} outside {}", fmt_bounds(min, max))),
        )
    }
}

fn check_keys(keys: &[String], value: &Value) -> (bool, Option<String>) {
    let Value::Object(map) = value else {
        let ty = json_type_name(value);
        return (false, Some(format!("json_has_keys does not apply to {ty}")));
    };
    let missing: Vec<&String> = keys.iter().filter(|k| !map.contains_key(*k)).collect();
    if missing.is_empty() {
        (true, None)
    } else {
        (false, Some(format!("missing keys: {missing:?}")))
    }
}

fn check_one_of(candidates: &[Value], value: &Value) -> (bool, Option<String>) {
    let canon = canonical_json(value);
    if candidates.iter().any(|c| canonical_json(c) == canon) {
        (true, None)
    } else {
        (false, Some("no candidate matched".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::check;
    use crate::model::{Property, Target};

    fn len_range(min: Option<u64>, max: Option<u64>) -> Property {
        Property::LenRange {
            target: Target::Output,
            min,
            max,
        }
    }

    fn regex_p(pattern: &str) -> Property {
        Property::Regex {
            target: Target::Output,
            pattern: pattern.to_string(),
        }
    }

    fn num_range(min: Option<f64>, max: Option<f64>) -> Property {
        Property::NumRange {
            target: Target::Output,
            min,
            max,
        }
    }

    fn has_keys(keys: &[&str]) -> Property {
        Property::JsonHasKeys {
            target: Target::Output,
            keys: keys.iter().map(|k| (*k).to_string()).collect(),
        }
    }

    fn one_of(values: Vec<Value>) -> Property {
        Property::OneOf {
            target: Target::Output,
            values,
        }
    }

    // --- LenRange ---

    #[test]
    fn len_range_counts_unicode_chars_not_bytes() {
        let s = "café!"; // 5 chars, 6 bytes
        assert_eq!(s.chars().count(), 5);
        assert_eq!(s.len(), 6);
        let o = check(&len_range(Some(5), Some(5)), &json!(s));
        assert!(o.passed);
        assert_eq!(o.detail, None);
        // a byte count of 6 would fail this bound
        assert!(check(&len_range(None, Some(5)), &json!(s)).passed);
    }

    #[test]
    fn len_range_counts_array_elements() {
        assert!(check(&len_range(Some(3), Some(3)), &json!([1, 2, 3])).passed);
        let o = check(&len_range(None, Some(2)), &json!([1, 2, 3]));
        assert!(!o.passed);
        assert_eq!(o.detail.as_deref(), Some("length 3 outside ..=2"));
    }

    #[test]
    fn len_range_bounds_are_inclusive() {
        assert!(check(&len_range(Some(2), None), &json!("ab")).passed);
        let o = check(&len_range(Some(3), None), &json!("ab"));
        assert!(!o.passed);
        assert_eq!(o.detail.as_deref(), Some("length 2 outside 3..="));
    }

    #[test]
    fn len_range_wrong_shape_fails() {
        let o = check(&len_range(Some(1), None), &json!(7));
        assert!(!o.passed);
        assert_eq!(
            o.detail.as_deref(),
            Some("len_range does not apply to number")
        );
        let o = check(&len_range(Some(1), None), &json!({"a": 1}));
        assert_eq!(
            o.detail.as_deref(),
            Some("len_range does not apply to object")
        );
        let o = check(&len_range(Some(1), None), &json!(null));
        assert_eq!(
            o.detail.as_deref(),
            Some("len_range does not apply to null")
        );
    }

    #[test]
    fn len_range_description_shapes() {
        let v = json!("x");
        assert_eq!(
            check(&len_range(Some(1), Some(200)), &v).description,
            "len_range(output, 1..=200)"
        );
        assert_eq!(
            check(&len_range(None, Some(200)), &v).description,
            "len_range(output, ..=200)"
        );
        assert_eq!(
            check(&len_range(Some(1), None), &v).description,
            "len_range(output, 1..=)"
        );
    }

    // --- Regex ---

    #[test]
    fn regex_is_search_not_anchored() {
        // search semantics: "b+" matches inside "abbbc"
        assert!(check(&regex_p("b+"), &json!("abbbc")).passed);
        // authors anchor explicitly for a full match
        assert!(!check(&regex_p("^b+$"), &json!("abbbc")).passed);
        assert!(check(&regex_p("^ab+c$"), &json!("abbbc")).passed);
    }

    #[test]
    fn regex_no_match_fails() {
        let o = check(&regex_p("z"), &json!("abc"));
        assert!(!o.passed);
        assert_eq!(o.detail.as_deref(), Some("no match"));
    }

    #[test]
    fn regex_wrong_shape_fails() {
        let o = check(&regex_p("a"), &json!(3));
        assert!(!o.passed);
        assert_eq!(o.detail.as_deref(), Some("regex does not apply to number"));
        let o = check(&regex_p("a"), &json!(["a"]));
        assert_eq!(o.detail.as_deref(), Some("regex does not apply to array"));
    }

    #[test]
    fn regex_compile_failure_is_a_failed_outcome_not_a_panic() {
        let o = check(&regex_p("("), &json!("anything"));
        assert!(!o.passed);
        let detail = o.detail.expect("compile failure carries a detail");
        assert!(detail.starts_with("pattern failed to compile"), "{detail}");
    }

    #[test]
    fn regex_description_shape() {
        let o = check(&regex_p("^a$"), &json!("a"));
        assert_eq!(o.description, "regex(output, \"^a$\")");
        assert!(o.passed);
        assert_eq!(o.detail, None);
    }

    // --- NumRange ---

    #[test]
    fn num_range_inclusive_bounds() {
        let p = num_range(Some(0.0), Some(100.0));
        assert!(check(&p, &json!(0)).passed);
        assert!(check(&p, &json!(100)).passed);
        assert!(check(&p, &json!(50.5)).passed);
        assert!(!check(&p, &json!(-1)).passed);
        let o = check(&p, &json!(150));
        assert!(!o.passed);
        assert_eq!(o.detail.as_deref(), Some("value 150 outside 0..=100"));
    }

    #[test]
    fn num_range_open_sides_and_u64() {
        assert!(check(&num_range(Some(0.0), None), &json!(u64::MAX)).passed);
        assert!(check(&num_range(None, Some(0.0)), &json!(-3.5)).passed);
    }

    #[test]
    fn num_range_wrong_shape_fails() {
        let o = check(&num_range(Some(0.0), None), &json!("50"));
        assert!(!o.passed);
        assert_eq!(
            o.detail.as_deref(),
            Some("num_range does not apply to string")
        );
    }

    #[test]
    fn num_range_description_formats_floats_plainly() {
        let v = json!(1);
        assert_eq!(
            check(&num_range(Some(0.0), Some(100.0)), &v).description,
            "num_range(output, 0..=100)"
        );
        assert_eq!(
            check(&num_range(Some(0.5), None), &v).description,
            "num_range(output, 0.5..=)"
        );
        assert_eq!(
            check(&num_range(None, Some(2.25)), &v).description,
            "num_range(output, ..=2.25)"
        );
    }

    // --- JsonHasKeys ---

    #[test]
    fn json_has_keys_pass() {
        let o = check(&has_keys(&["a", "b"]), &json!({"a": 1, "b": 2, "c": 3}));
        assert!(o.passed);
        assert_eq!(o.detail, None);
    }

    #[test]
    fn json_has_keys_detail_lists_missing() {
        let o = check(&has_keys(&["a", "b", "c"]), &json!({"a": 1}));
        assert!(!o.passed);
        assert_eq!(o.detail.as_deref(), Some(r#"missing keys: ["b", "c"]"#));
    }

    #[test]
    fn json_has_keys_wrong_shape_fails() {
        let o = check(&has_keys(&["a"]), &json!([{"a": 1}]));
        assert!(!o.passed);
        assert_eq!(
            o.detail.as_deref(),
            Some("json_has_keys does not apply to array")
        );
        let o = check(&has_keys(&["a"]), &json!("a"));
        assert_eq!(
            o.detail.as_deref(),
            Some("json_has_keys does not apply to string")
        );
    }

    #[test]
    fn json_has_keys_description_shape() {
        let o = check(&has_keys(&["a", "b"]), &json!({}));
        assert_eq!(o.description, r#"json_has_keys(output, ["a", "b"])"#);
    }

    // --- OneOf ---

    #[test]
    fn one_of_matches_any_candidate() {
        let p = one_of(vec![json!(1), json!(2), json!(3)]);
        let o = check(&p, &json!(2));
        assert!(o.passed);
        assert_eq!(o.detail, None);
    }

    #[test]
    fn one_of_object_key_order_is_irrelevant() {
        let p = one_of(vec![json!({"a": 1, "b": {"x": true, "y": null}})]);
        // same object, keys in reverse document order
        let v: Value =
            serde_json::from_str(r#"{"b": {"y": null, "x": true}, "a": 1}"#).expect("valid json");
        assert!(check(&p, &v).passed);
    }

    #[test]
    fn one_of_no_match_fails() {
        let p = one_of(vec![json!(1), json!(2), json!(3)]);
        let o = check(&p, &json!(4));
        assert!(!o.passed);
        assert_eq!(o.detail.as_deref(), Some("no candidate matched"));
        // a type-mismatched value fails, it does not skip
        assert!(!check(&p, &json!("2")).passed);
        assert!(!check(&p, &json!(null)).passed);
    }

    #[test]
    fn one_of_description_counts_candidates() {
        let p = one_of(vec![json!(1), json!(2), json!(3)]);
        assert_eq!(check(&p, &json!(1)).description, "one_of(output, 3 values)");
    }
}
