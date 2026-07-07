//! Region synthesis — a recorded chain of spans compiled into ONE pipeline
//! (spec/synthesis.md §8, ADR-0015).
//!
//! Every arrow in the chain is its own synthesis problem over witnessed
//! value pairs: each STAGE (a span's input → output) and each GLUE edge
//! (one span's output → the next span's input — the agent code between
//! calls, which traces never record as code but always record as values).
//! Glue whose witnessed pairs are all identical values is identity and is
//! OMITTED from the pipeline (the DSL has no identity program on purpose);
//! anything else must synthesize or the region honestly refuses, naming
//! the exact edge that failed.
//!
//! The assembled [`auto_dsl::Pipeline`] is evidence-bounded exactly like a
//! single synthesized program: it reproduces the witnesses, and the emit
//! gate still replays every recorded end-to-end chain differentially.

use auto_dsl::{Pipeline, Program, Stage};
use serde_json::Value;

use crate::extraction::{Observation, SearchBudget, SearchOutcome, synthesize};

/// The honest outcomes of region synthesis.
#[derive(Debug, Clone, PartialEq)]
pub enum RegionOutcome {
    Found {
        pipeline: Pipeline,
        /// chain stages synthesized (always == chain length)
        stages: usize,
        /// glue edges that needed a synthesized program
        glue_synthesized: usize,
        /// glue edges witnessed as identity and omitted
        glue_identity: usize,
    },
    /// one edge could not be synthesized; nothing is assembled
    EdgeRefused {
        /// e.g. `stage route` or `glue extract->route`
        edge: String,
        /// the search outcome, honestly ("budget exhausted (...)", "conflicting observations")
        detail: String,
    },
}

/// Synthesize one edge's function from its witnessed pairs.
fn synthesize_edge(
    edge: String,
    pairs: &[(Value, Value)],
    budget: SearchBudget,
) -> Result<Program, RegionOutcome> {
    let observations: Vec<Observation> = pairs
        .iter()
        .map(|(input, output)| Observation {
            input: input.clone(),
            output: output.clone(),
        })
        .collect();
    match synthesize(&observations, budget) {
        SearchOutcome::Found(synthesis) => Ok(synthesis.program),
        SearchOutcome::BudgetExhausted {
            states_explored,
            depth_reached,
        } => Err(RegionOutcome::EdgeRefused {
            edge,
            detail: format!(
                "budget exhausted ({states_explored} state(s), depth {depth_reached}) — \
                 no fitting program in the v0 DSL"
            ),
        }),
        SearchOutcome::ConflictingObservations => Err(RegionOutcome::EdgeRefused {
            edge,
            detail: "conflicting observations (same value in, different values out)".to_owned(),
        }),
    }
}

/// True when every witnessed pair carries the value through unchanged.
fn is_identity(pairs: &[(Value, Value)]) -> bool {
    !pairs.is_empty() && pairs.iter().all(|(a, b)| a == b)
}

/// Compile a gathered region chain into a pipeline: stage, glue, stage,
/// glue, … with identity glue omitted. `chain` is the (kind, name)
/// signature; `stage_pairs[k]` / `glue_pairs[k]` are the witnessed value
/// pairs (`auto_backend::differential::gather_region`). Each edge gets the
/// full `budget` — a region's search cost is per-edge, stated in the notes.
pub fn synthesize_region(
    chain: &[(String, String)],
    stage_pairs: &[Vec<(Value, Value)>],
    glue_pairs: &[Vec<(Value, Value)>],
    budget: SearchBudget,
) -> RegionOutcome {
    assert_eq!(chain.len(), stage_pairs.len(), "one pair set per stage");
    assert_eq!(
        glue_pairs.len(),
        chain.len().saturating_sub(1),
        "one pair set per adjacent stage pair"
    );

    let mut stages = Vec::new();
    let mut glue_synthesized = 0usize;
    let mut glue_identity = 0usize;

    for (position, (kind, name)) in chain.iter().enumerate() {
        if kind == "tool_call" {
            // a tool stage is a capability boundary, never synthesized: the
            // artifact calls the declared tool through its import (ADR-0017)
            stages.push(Stage::Tool { name: name.clone() });
        } else {
            match synthesize_edge(format!("stage {name}"), &stage_pairs[position], budget) {
                Ok(program) => stages.push(Stage::Program(program)),
                Err(refused) => return refused,
            }
        }
        if position < glue_pairs.len() {
            let next = &chain[position + 1].1;
            let pairs = &glue_pairs[position];
            if is_identity(pairs) {
                glue_identity += 1;
                continue;
            }
            match synthesize_edge(format!("glue {name}->{next}"), pairs, budget) {
                Ok(program) => {
                    stages.push(Stage::Program(program));
                    glue_synthesized += 1;
                }
                Err(refused) => return refused,
            }
        }
    }

    RegionOutcome::Found {
        pipeline: Pipeline::new(stages),
        stages: chain.len(),
        glue_synthesized,
        glue_identity,
    }
}

#[cfg(test)]
mod tests {
    use auto_dsl::{eval_pipeline, no_tools};
    use serde_json::json;

    use super::*;

    fn chain() -> Vec<(String, String)> {
        vec![
            ("model_call".into(), "extract".into()),
            ("model_call".into(), "route".into()),
            ("model_call".into(), "format".into()),
        ]
    }

    /// per-position witnessed value pairs (stage or glue)
    type Pairs = Vec<Vec<(Value, Value)>>;

    /// Honest fixtures: pairs computed BY the rules the stages implement
    /// (extract = lowercase words joined; route = first word; format =
    /// uppercase), over two witnessed docs.
    fn fixture() -> (Pairs, Pairs) {
        let docs = ["Beta Alpha", "Delta Gamma Zeta"];
        let mut stage_pairs = vec![Vec::new(), Vec::new(), Vec::new()];
        let mut glue_pairs = vec![Vec::new(), Vec::new()];
        for doc in docs {
            let extracted = doc
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let routed = extracted.split_whitespace().next().unwrap().to_owned();
            let formatted = routed.to_uppercase();
            stage_pairs[0].push((json!({ "doc": doc }), json!(extracted)));
            stage_pairs[1].push((json!(extracted), json!(routed)));
            stage_pairs[2].push((json!(routed), json!(formatted)));
            glue_pairs[0].push((json!(extracted), json!(extracted))); // identity
            glue_pairs[1].push((json!(routed), json!(routed))); // identity
        }
        (stage_pairs, glue_pairs)
    }

    #[test]
    fn chain_of_stages_with_identity_glue_synthesizes_and_composes() {
        let (stage_pairs, glue_pairs) = fixture();
        let outcome =
            synthesize_region(&chain(), &stage_pairs, &glue_pairs, SearchBudget::default());
        let RegionOutcome::Found {
            pipeline,
            stages,
            glue_synthesized,
            glue_identity,
        } = outcome
        else {
            panic!("expected Found, got {outcome:?}");
        };
        assert_eq!(stages, 3);
        assert_eq!(glue_identity, 2);
        assert_eq!(glue_synthesized, 0);
        assert_eq!(pipeline.stages.len(), 3, "identity glue omitted");
        // the assembled pipeline reproduces a witnessed chain end-to-end
        assert_eq!(
            eval_pipeline(&pipeline, &json!({"doc": "Beta Alpha"}), &mut no_tools),
            Ok(json!("BETA"))
        );
        // and behaves on a doc it never witnessed (evidence-bounded claim
        // only — the emit gate is what verifies, this is a smoke check)
        assert_eq!(
            eval_pipeline(&pipeline, &json!({"doc": "Omega Psi"}), &mut no_tools),
            Ok(json!("OMEGA"))
        );
    }

    #[test]
    fn non_identity_glue_is_synthesized_into_the_pipeline() {
        // one stage, one glue that uppercases, one final stage taking the
        // first word: glue must become a real program
        let chain: Vec<(String, String)> = vec![
            ("model_call".into(), "a".into()),
            ("model_call".into(), "b".into()),
        ];
        let stage_pairs = vec![
            vec![
                (json!({"doc": "x y"}), json!("x y")),
                (json!({"doc": "p q"}), json!("p q")),
            ],
            vec![(json!("X Y"), json!("X")), (json!("P Q"), json!("P"))],
        ];
        let glue_pairs = vec![vec![
            (json!("x y"), json!("X Y")),
            (json!("p q"), json!("P Q")),
        ]];
        let outcome = synthesize_region(&chain, &stage_pairs, &glue_pairs, SearchBudget::default());
        let RegionOutcome::Found {
            pipeline,
            glue_synthesized,
            glue_identity,
            ..
        } = outcome
        else {
            panic!("expected Found, got {outcome:?}");
        };
        assert_eq!(glue_synthesized, 1);
        assert_eq!(glue_identity, 0);
        assert_eq!(pipeline.stages.len(), 3);
        assert_eq!(
            eval_pipeline(&pipeline, &json!({"doc": "m n"}), &mut no_tools),
            Ok(json!("M"))
        );
    }

    #[test]
    fn an_unsynthesizable_edge_refuses_naming_it() {
        // glue that requires constructing an object — inexpressible in v0
        let chain: Vec<(String, String)> = vec![
            ("model_call".into(), "a".into()),
            ("model_call".into(), "b".into()),
        ];
        let stage_pairs = vec![
            vec![
                (json!({"doc": "x"}), json!("x")),
                (json!({"doc": "y"}), json!("y")),
            ],
            vec![
                (json!({"label": "x"}), json!("x")),
                (json!({"label": "y"}), json!("y")),
            ],
        ];
        let glue_pairs = vec![vec![
            (json!("x"), json!({"label": "x"})),
            (json!("y"), json!({"label": "y"})),
        ]];
        let outcome = synthesize_region(
            &chain,
            &stage_pairs,
            &glue_pairs,
            SearchBudget {
                max_states: 20_000,
                max_depth: 4,
            },
        );
        let RegionOutcome::EdgeRefused { edge, detail } = outcome else {
            panic!("expected EdgeRefused, got {outcome:?}");
        };
        assert_eq!(edge, "glue a->b");
        assert!(detail.contains("budget exhausted"), "{detail}");
    }

    #[test]
    fn tool_stages_pass_through_as_capability_boundaries() {
        // extract (program) -> lookup (TOOL, never synthesized) -> format
        // (program); glue identity throughout. Honest fixture: the tool's
        // recorded pairs exist in the gather but no synthesis touches them.
        let chain: Vec<(String, String)> = vec![
            ("model_call".into(), "extract".into()),
            ("tool_call".into(), "lookup".into()),
            ("model_call".into(), "format".into()),
        ];
        let stage_pairs = vec![
            vec![
                (json!({"doc": "Beta Alpha"}), json!("beta alpha")),
                (json!({"doc": "Delta Gamma"}), json!("delta gamma")),
            ],
            vec![
                (json!("beta alpha"), json!("team-b")),
                (json!("delta gamma"), json!("team-d")),
            ],
            vec![
                (json!("team-b"), json!("TEAM-B")),
                (json!("team-d"), json!("TEAM-D")),
            ],
        ];
        let glue_pairs = vec![
            vec![
                (json!("beta alpha"), json!("beta alpha")),
                (json!("delta gamma"), json!("delta gamma")),
            ],
            vec![
                (json!("team-b"), json!("team-b")),
                (json!("team-d"), json!("team-d")),
            ],
        ];
        let outcome = synthesize_region(&chain, &stage_pairs, &glue_pairs, SearchBudget::default());
        let RegionOutcome::Found {
            pipeline,
            stages,
            glue_identity,
            ..
        } = outcome
        else {
            panic!("expected Found, got {outcome:?}");
        };
        assert_eq!(stages, 3);
        assert_eq!(glue_identity, 2);
        assert_eq!(pipeline.stages.len(), 3);
        assert_eq!(pipeline.capabilities(), vec!["lookup".to_owned()]);
        // the assembled pipeline runs against a labeled tool seam
        let mut tool =
            |name: &str, input: &serde_json::Value| -> Result<serde_json::Value, String> {
                assert_eq!(name, "lookup");
                Ok(json!(format!(
                    "team-{}",
                    input.as_str().unwrap().chars().next().unwrap()
                )))
            };
        assert_eq!(
            eval_pipeline(&pipeline, &json!({"doc": "Beta Alpha"}), &mut tool),
            Ok(json!("TEAM-B"))
        );
    }

    #[test]
    fn conflicting_stage_pairs_refuse_with_the_stage_name() {
        let chain: Vec<(String, String)> = vec![("model_call".into(), "only".into())];
        let stage_pairs = vec![vec![
            (json!("same"), json!("one")),
            (json!("same"), json!("two")),
        ]];
        let outcome = synthesize_region(&chain, &stage_pairs, &[], SearchBudget::default());
        assert_eq!(
            outcome,
            RegionOutcome::EdgeRefused {
                edge: "stage only".into(),
                detail: "conflicting observations (same value in, different values out)".into(),
            }
        );
    }
}
