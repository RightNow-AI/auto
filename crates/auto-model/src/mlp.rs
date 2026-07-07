//! Neural distilled-model format v0: a single-hidden-layer MLP over the
//! frozen char-trigram features (spec/distillation.md §mlp).
//!
//! The smallest honest neural specialist: dense float weights trained by the
//! torch trainer (locally or on Modal GPU), exported as plain JSON, executed
//! by ~30 lines of matmul — natively here and compiled to wasm in
//! `mlp-interpreter`. One implementation, two compilations, parity-gated
//! like every artifact.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Features, featurize};

/// MLP wire-format version; readers accept exactly this. Bump with an ADR.
pub const MLP_VERSION: u32 = 0;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    mlp_version: u32,
    features: Features,
    /// row-major [hidden][buckets]
    hidden_weights: Vec<Vec<f64>>,
    /// [hidden]
    hidden_bias: Vec<f64>,
    /// row-major [classes][hidden]
    out_weights: Vec<Vec<f64>>,
    /// [classes]
    out_bias: Vec<f64>,
    /// output labels, argmax-indexed
    classes: Vec<String>,
}

/// A loaded MLP.
#[derive(Debug, Clone, PartialEq)]
pub struct Mlp {
    pub features: Features,
    pub hidden_weights: Vec<Vec<f64>>,
    pub hidden_bias: Vec<f64>,
    pub out_weights: Vec<Vec<f64>>,
    pub out_bias: Vec<f64>,
    pub classes: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MlpError {
    #[error("invalid mlp json: {0}")]
    BadJson(String),
    #[error("unsupported mlp_version {found}; this build reads exactly 0")]
    UnsupportedVersion { found: u32 },
    #[error("unknown feature kind {0:?}")]
    UnknownFeatureKind(String),
    #[error("dimension mismatch: {0}")]
    BadShape(String),
    #[error("weights must be finite: {0}")]
    NotFinite(String),
}

impl Mlp {
    /// Strict parse + shape/finiteness validation.
    pub fn from_json(text: &str) -> Result<Self, MlpError> {
        let wire: Wire =
            serde_json::from_str(text).map_err(|e| MlpError::BadJson(e.to_string()))?;
        if wire.mlp_version != MLP_VERSION {
            return Err(MlpError::UnsupportedVersion {
                found: wire.mlp_version,
            });
        }
        if wire.features.kind != "char_trigram_fnv1a" {
            return Err(MlpError::UnknownFeatureKind(wire.features.kind));
        }
        let buckets = wire.features.buckets as usize;
        let hidden = wire.hidden_weights.len();
        let classes = wire.classes.len();
        if hidden == 0 || classes < 2 {
            return Err(MlpError::BadShape(format!(
                "hidden={hidden}, classes={classes}; need hidden >= 1 and classes >= 2"
            )));
        }
        if wire.hidden_bias.len() != hidden {
            return Err(MlpError::BadShape(format!(
                "hidden_bias len {} != hidden {hidden}",
                wire.hidden_bias.len()
            )));
        }
        for (i, row) in wire.hidden_weights.iter().enumerate() {
            if row.len() != buckets {
                return Err(MlpError::BadShape(format!(
                    "hidden_weights[{i}] len {} != buckets {buckets}",
                    row.len()
                )));
            }
        }
        if wire.out_weights.len() != classes || wire.out_bias.len() != classes {
            return Err(MlpError::BadShape(format!(
                "out_weights/out_bias lens {}/{} != classes {classes}",
                wire.out_weights.len(),
                wire.out_bias.len()
            )));
        }
        for (i, row) in wire.out_weights.iter().enumerate() {
            if row.len() != hidden {
                return Err(MlpError::BadShape(format!(
                    "out_weights[{i}] len {} != hidden {hidden}",
                    row.len()
                )));
            }
        }
        let all = wire
            .hidden_weights
            .iter()
            .chain(wire.out_weights.iter())
            .flatten()
            .chain(wire.hidden_bias.iter())
            .chain(wire.out_bias.iter());
        for (i, v) in all.enumerate() {
            if !v.is_finite() {
                return Err(MlpError::NotFinite(format!("weight #{i} is {v}")));
            }
        }
        Ok(Self {
            features: wire.features,
            hidden_weights: wire.hidden_weights,
            hidden_bias: wire.hidden_bias,
            out_weights: wire.out_weights,
            out_bias: wire.out_bias,
            classes: wire.classes,
        })
    }

    /// Canonical JSON (sorted keys).
    pub fn to_json(&self) -> String {
        let wire = Wire {
            mlp_version: MLP_VERSION,
            features: self.features.clone(),
            hidden_weights: self.hidden_weights.clone(),
            hidden_bias: self.hidden_bias.clone(),
            out_weights: self.out_weights.clone(),
            out_bias: self.out_bias.clone(),
            classes: self.classes.clone(),
        };
        let value = serde_json::to_value(&wire).expect("mlp serialization cannot fail");
        serde_json::to_string(&value).expect("value serialization cannot fail")
    }
}

/// Run the MLP: extract text, featurize, relu hidden layer, argmax logits.
/// Ties break toward the lowest class index (deterministic; documented).
pub fn infer_mlp(mlp: &Mlp, input: &Value) -> Result<Value, crate::InferError> {
    let text = match &mlp.features.input_field {
        Some(field) => input
            .as_object()
            .and_then(|o| o.get(field))
            .ok_or_else(|| crate::InferError::MissingField(field.clone()))?,
        None => input,
    };
    let Value::String(text) = text else {
        return Err(crate::InferError::NotText(match text {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::String(_) => unreachable!("string handled above"),
        }));
    };
    let x = featurize(text, mlp.features.buckets);
    let hidden: Vec<f64> = mlp
        .hidden_weights
        .iter()
        .zip(&mlp.hidden_bias)
        .map(|(row, b)| {
            let z: f64 = row.iter().zip(&x).map(|(w, v)| w * v).sum::<f64>() + b;
            z.max(0.0) // relu
        })
        .collect();
    let mut best = 0_usize;
    let mut best_logit = f64::NEG_INFINITY;
    for (i, (row, b)) in mlp.out_weights.iter().zip(&mlp.out_bias).enumerate() {
        let logit: f64 = row.iter().zip(&hidden).map(|(w, h)| w * h).sum::<f64>() + b;
        if logit > best_logit {
            best_logit = logit;
            best = i;
        }
    }
    Ok(Value::String(mlp.classes[best].clone()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// 2-bucket, 1-hidden, 2-class MLP fully hand-computable: hidden = relu(
    /// x0 - x1 ), out logits = [h, -h] + [0, 0.1] → "a" iff h > 0.1 else "b".
    fn tiny() -> Mlp {
        Mlp {
            features: Features {
                kind: "char_trigram_fnv1a".into(),
                buckets: 2,
                input_field: None,
            },
            hidden_weights: vec![vec![1.0, -1.0]],
            hidden_bias: vec![0.0],
            out_weights: vec![vec![1.0], vec![-1.0]],
            out_bias: vec![0.0, 0.1],
            classes: vec!["a".into(), "b".into()],
        }
    }

    #[test]
    fn hand_computed_forward_pass() {
        let m = tiny();
        // "aaa" → one trigram, lands in some bucket; both cases covered by
        // symmetric inputs. Compute expectations from featurize directly.
        let x_aaa = featurize("aaa", 2);
        let h = (x_aaa[0] - x_aaa[1]).max(0.0);
        let expect = if h > 0.1 { "a" } else { "b" };
        assert_eq!(infer_mlp(&m, &json!("aaa")), Ok(json!(expect)));
    }

    #[test]
    fn ties_break_low_index() {
        let mut m = tiny();
        m.out_weights = vec![vec![0.0], vec![0.0]];
        m.out_bias = vec![0.5, 0.5];
        assert_eq!(infer_mlp(&m, &json!("anything")), Ok(json!("a")));
    }

    #[test]
    fn json_roundtrip_and_strictness() {
        let m = tiny();
        let text = m.to_json();
        assert_eq!(Mlp::from_json(&text).unwrap(), m);
        assert!(matches!(
            Mlp::from_json(&text.replace("\"mlp_version\":0", "\"mlp_version\":1")),
            Err(MlpError::UnsupportedVersion { found: 1 })
        ));
        assert!(matches!(
            Mlp::from_json(&text.replace("[1.0,-1.0]", "[1.0]")),
            Err(MlpError::BadShape(_))
        ));
    }

    #[test]
    fn wrong_shaped_input_is_typed_error() {
        let mut m = tiny();
        m.features.input_field = Some("text".into());
        assert!(matches!(
            infer_mlp(&m, &json!({"other": "x"})),
            Err(crate::InferError::MissingField(_))
        ));
    }
}
