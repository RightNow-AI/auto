//! Distilled-model format v0 (spec/distillation.md).
//!
//! A [`Model`] is a feature spec plus a single decision tree — the smallest
//! honest distillation target ("gradient boosting when it wins" and neural
//! specialists are recorded upgrades, ADR-0006). The python trainer emits
//! this format; this crate infers over it, natively (tests, tooling) and
//! compiled to wasm inside artifacts (`model-interpreter`).
//!
//! **The feature spec is frozen and must match the trainer bit-for-bit**:
//! lowercase the text (unicode), slide a 3-char window over its chars
//! (including spaces), hash each trigram's utf-8 bytes with FNV-1a 32-bit
//! (offset 2166136261, prime 16777619), take `hash % buckets` as the feature
//! index, count occurrences (f64 counts). Split rule: `count <= threshold`
//! goes left (sklearn semantics).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod mlp;

pub use mlp::{MLP_VERSION, Mlp, MlpError, infer_mlp};

/// Model wire-format version; readers accept exactly this. Bump with an ADR.
pub const MODEL_VERSION: u32 = 0;

/// Feature extraction spec. v0 has exactly one kind; the `kind` field exists
/// so a new featurizer is a loud format change, not a silent reinterpretation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Features {
    /// must be "char_trigram_fnv1a"
    pub kind: String,
    /// feature-vector width; indices are fnv1a(trigram) % buckets
    pub buckets: u32,
    /// object field holding the text; None = the input IS the text
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_field: Option<String>,
}

/// One tree node: an internal split or a leaf label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Node {
    #[serde(rename = "split")]
    Split {
        feature: u32,
        threshold: f64,
        left: u32,
        right: u32,
    },
    #[serde(rename = "leaf")]
    Leaf { label: String },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    model_version: u32,
    features: Features,
    /// node 0 is the root
    nodes: Vec<Node>,
}

/// A loaded model.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub features: Features,
    pub nodes: Vec<Node>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelError {
    #[error("invalid model json: {0}")]
    BadJson(String),
    #[error("unsupported model_version {found}; this build reads exactly 0")]
    UnsupportedVersion { found: u32 },
    #[error("unknown feature kind {0:?}")]
    UnknownFeatureKind(String),
    #[error("buckets must be > 0")]
    ZeroBuckets,
    #[error("model has no nodes")]
    Empty,
    #[error("node {node} points past the node table (len {len})")]
    BadNodeRef { node: u32, len: usize },
    #[error("node {node} splits on feature {feature}, past {buckets} buckets")]
    BadFeatureRef {
        node: u32,
        feature: u32,
        buckets: u32,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InferError {
    #[error("input is not an object with field {0:?}")]
    MissingField(String),
    #[error("input text is not a string (found {0})")]
    NotText(&'static str),
    #[error("tree walk exceeded the node count (cycle)")]
    Cycle,
}

impl Model {
    /// Strict parse + structural validation (refs in range, version exact).
    pub fn from_json(text: &str) -> Result<Self, ModelError> {
        let wire: Wire =
            serde_json::from_str(text).map_err(|e| ModelError::BadJson(e.to_string()))?;
        if wire.model_version != MODEL_VERSION {
            return Err(ModelError::UnsupportedVersion {
                found: wire.model_version,
            });
        }
        if wire.features.kind != "char_trigram_fnv1a" {
            return Err(ModelError::UnknownFeatureKind(wire.features.kind));
        }
        if wire.features.buckets == 0 {
            return Err(ModelError::ZeroBuckets);
        }
        if wire.nodes.is_empty() {
            return Err(ModelError::Empty);
        }
        let len = wire.nodes.len();
        for (i, node) in wire.nodes.iter().enumerate() {
            if let Node::Split {
                feature,
                left,
                right,
                ..
            } = node
            {
                for child in [*left, *right] {
                    if child as usize >= len {
                        return Err(ModelError::BadNodeRef {
                            node: u32::try_from(i).expect("node index fits"),
                            len,
                        });
                    }
                }
                if *feature >= wire.features.buckets {
                    return Err(ModelError::BadFeatureRef {
                        node: u32::try_from(i).expect("node index fits"),
                        feature: *feature,
                        buckets: wire.features.buckets,
                    });
                }
            }
        }
        Ok(Self {
            features: wire.features,
            nodes: wire.nodes,
        })
    }

    /// Canonical JSON (sorted keys — serde_json's map is ordered).
    pub fn to_json(&self) -> String {
        let wire = Wire {
            model_version: MODEL_VERSION,
            features: self.features.clone(),
            nodes: self.nodes.clone(),
        };
        let value = serde_json::to_value(&wire).expect("model serialization cannot fail");
        serde_json::to_string(&value).expect("value serialization cannot fail")
    }
}

/// FNV-1a 32-bit over bytes. Frozen: the python trainer implements the same
/// constants; a drift here silently reshuffles every feature.
pub fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 2_166_136_261;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

/// The sorted, deduplicated fnv1a hashes of a text's char trigrams (same
/// trigram rule as [`featurize`], unbucketed). The runtime guard's witness
/// sketch (spec/runtime.md) — set semantics, not counts.
pub fn trigram_hashes(text: &str) -> Vec<u32> {
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    if chars.len() < 3 {
        return Vec::new();
    }
    let mut buf = String::new();
    let mut set = std::collections::BTreeSet::new();
    for window in chars.windows(3) {
        buf.clear();
        buf.extend(window);
        set.insert(fnv1a_32(buf.as_bytes()));
    }
    set.into_iter().collect()
}

/// Featurize text per the frozen spec: lowercase, char trigrams (unicode
/// chars, spaces included), fnv1a % buckets, occurrence counts.
pub fn featurize(text: &str, buckets: u32) -> Vec<f64> {
    let mut counts = vec![0.0_f64; buckets as usize];
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    if chars.len() < 3 {
        return counts;
    }
    let mut buf = String::new();
    for window in chars.windows(3) {
        buf.clear();
        buf.extend(window);
        let index = fnv1a_32(buf.as_bytes()) % buckets;
        counts[index as usize] += 1.0;
    }
    counts
}

/// Run the model on an input value: extract text, featurize, walk the tree.
/// Total: every failure is typed; the walk is cycle-bounded.
pub fn infer(model: &Model, input: &Value) -> Result<Value, InferError> {
    let text = match &model.features.input_field {
        Some(field) => input
            .as_object()
            .and_then(|o| o.get(field))
            .ok_or_else(|| InferError::MissingField(field.clone()))?,
        None => input,
    };
    let Value::String(text) = text else {
        return Err(InferError::NotText(match text {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::String(_) => unreachable!("string handled above"),
        }));
    };
    let counts = featurize(text, model.features.buckets);
    let mut node = 0_usize;
    // any walk longer than the node count revisited a node: cycle guard
    for _ in 0..model.nodes.len() {
        match &model.nodes[node] {
            Node::Leaf { label } => return Ok(Value::String(label.clone())),
            Node::Split {
                feature,
                threshold,
                left,
                right,
            } => {
                node = if counts[*feature as usize] <= *threshold {
                    *left as usize
                } else {
                    *right as usize
                };
            }
        }
    }
    Err(InferError::Cycle)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn fnv1a_pinned_vectors() {
        // pinned: the python trainer asserts the same three vectors
        assert_eq!(fnv1a_32(b""), 2_166_136_261);
        assert_eq!(fnv1a_32(b"a"), 0xE40C_292C);
        assert_eq!(fnv1a_32(b"abc"), 0x1A47_E90B);
    }

    #[test]
    fn featurize_counts_trigrams() {
        let counts = featurize("AbAb", 8);
        // "abab" -> trigrams "aba","bab"
        assert_eq!(counts.iter().sum::<f64>(), 2.0);
        assert!(featurize("ab", 8).iter().sum::<f64>() == 0.0);
    }

    fn tiny_model() -> Model {
        // one split on the bucket of trigram "foo": present -> "hit" else "miss"
        let bucket = fnv1a_32(b"foo") % 16;
        Model {
            features: Features {
                kind: "char_trigram_fnv1a".into(),
                buckets: 16,
                input_field: Some("text".into()),
            },
            nodes: vec![
                Node::Split {
                    feature: bucket,
                    threshold: 0.5,
                    left: 1,
                    right: 2,
                },
                Node::Leaf {
                    label: "miss".into(),
                },
                Node::Leaf {
                    label: "hit".into(),
                },
            ],
        }
    }

    #[test]
    fn inference_walks_the_tree() {
        let m = tiny_model();
        assert_eq!(infer(&m, &json!({"text": "xx foo xx"})), Ok(json!("hit")));
        assert_eq!(infer(&m, &json!({"text": "nothing"})), Ok(json!("miss")));
        assert_eq!(
            infer(&m, &json!({"other": "foo"})),
            Err(InferError::MissingField("text".into()))
        );
        assert_eq!(
            infer(&m, &json!({"text": 42})),
            Err(InferError::NotText("number"))
        );
    }

    #[test]
    fn json_roundtrip_and_strictness() {
        let m = tiny_model();
        let text = m.to_json();
        assert!(text.starts_with("{\"features\":"));
        assert_eq!(Model::from_json(&text).unwrap(), m);

        assert!(matches!(
            Model::from_json(&text.replace("\"model_version\":0", "\"model_version\":1")),
            Err(ModelError::UnsupportedVersion { found: 1 })
        ));
        assert!(matches!(
            Model::from_json(&text.replace("char_trigram_fnv1a", "word2vec")),
            Err(ModelError::UnknownFeatureKind(_))
        ));
        // out-of-range child
        let bad = text.replace("\"left\":1", "\"left\":9");
        assert!(matches!(
            Model::from_json(&bad),
            Err(ModelError::BadNodeRef { .. })
        ));
    }

    #[test]
    fn cycle_guard_is_an_error_not_a_hang() {
        let m = Model {
            features: Features {
                kind: "char_trigram_fnv1a".into(),
                buckets: 4,
                input_field: None,
            },
            nodes: vec![Node::Split {
                feature: 0,
                threshold: 0.5,
                left: 0,
                right: 0,
            }],
        };
        assert_eq!(infer(&m, &json!("abc")), Err(InferError::Cycle));
    }
}
