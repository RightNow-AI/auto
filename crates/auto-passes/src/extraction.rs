//! Enumerative symbolic extraction — the S4 synthesizer (spec/synthesis.md).
//!
//! Bottom-up BFS over straight-line `auto-dsl` pipelines with
//! observational-equivalence dedupe: a search state is the vector of
//! register values across all distinct observed inputs, so two pipelines
//! that agree on every witness are one state. Deterministic by construction
//! (BTreeMap-ordered witnesses and frontiers plus a fixed documented
//! op-instance order): the same observations always synthesize the same
//! program, shortest first.
//!
//! This is v0 of the constitution's CEGIS extraction pass: proposals are
//! exhaustive enumeration over a mined parameter space, verification is
//! exact evaluation against the witnesses. LLM-guided proposal generation
//! is future work.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

// `eval` is auto-dsl's total interpreter over the closed `Op` enum — pure
// data transformations, no code execution (see its security note)
use auto_dsl::{Op, Program, eval};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// One distinct recorded observation (canonical input → canonical output).
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub input: Value,
    pub output: Value,
}

/// Search limits. Exhaustion is an honest outcome, not an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchBudget {
    pub max_depth: usize,
    pub max_states: usize,
}

impl Default for SearchBudget {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_states: 300_000,
        }
    }
}

/// A successful synthesis with its provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct Synthesis {
    pub program: auto_dsl::Program,
    pub distinct_inputs: usize,
    pub states_explored: usize,
    pub depth_reached: usize,
}

/// The three honest outcomes of a search.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchOutcome {
    Found(Synthesis),
    /// no fitting program within budget
    BudgetExhausted {
        states_explored: usize,
        depth_reached: usize,
    },
    /// observations conflict (same input, different outputs) — nothing to
    /// synthesize; the signature is not deterministic
    ConflictingObservations,
}

/// Search for a DSL program that reproduces every distinct observation.
///
/// Observations are deduped by canonical input (`serde_json::to_string`;
/// object keys are sorted). One input witnessed with two different outputs
/// is [`SearchOutcome::ConflictingObservations`]. If every distinct input
/// maps to the identical output — including the one-witness case, where
/// constant behavior is indistinguishable from computation — the depth-1
/// [`Op::ConstOut`] program is returned without searching (the budget is
/// not consulted). Otherwise: bottom-up BFS; level k+1 applies every
/// admissible op instance (fixed candidate order, see `static_instances`,
/// then the mined `Add` delta) to every level-k state; an op that
/// eval-errors on any witness prunes that candidate; states are deduped by
/// digest of their canonical-register vector (observational equivalence).
/// The first state equal to the output vector wins — shortest program
/// first, ties resolved by expansion order (ascending parent-state digest,
/// then instance order). `ConstOut` is never proposed inside the search, so
/// pipelines cannot smuggle in memo-tables.
///
/// Counters: `states_explored` counts distinct states inserted (start state
/// included); `depth_reached` is the deepest inserted level and equals the
/// program length on success. No observations at all, an exhausted budget,
/// or a reachable space that closes below the budget each report
/// [`SearchOutcome::BudgetExhausted`] — honestly "no fitting program
/// found", never a fabricated one. A found program is evidence-bounded: it
/// reproduces the witnesses; parity beyond them is the verification pass's
/// job.
pub fn synthesize(observations: &[Observation], budget: SearchBudget) -> SearchOutcome {
    let mut witnesses: BTreeMap<String, Witness> = BTreeMap::new();
    for obs in observations {
        let canon_input = canonical(&obs.input);
        let canon_output = canonical(&obs.output);
        match witnesses.entry(canon_input) {
            Entry::Occupied(existing) => {
                if existing.get().canon_output != canon_output {
                    return SearchOutcome::ConflictingObservations;
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(Witness {
                    input: obs.input.clone(),
                    output: obs.output.clone(),
                    canon_output,
                });
            }
        }
    }
    if witnesses.is_empty() {
        return SearchOutcome::BudgetExhausted {
            states_explored: 0,
            depth_reached: 0,
        };
    }

    // witnesses in ascending canonical-input order (BTreeMap): the register
    // vector layout is a pure function of the observation SET
    let mut inputs = Vec::with_capacity(witnesses.len());
    let mut outputs = Vec::with_capacity(witnesses.len());
    let mut target = Vec::with_capacity(witnesses.len());
    for witness in witnesses.into_values() {
        inputs.push(witness.input);
        outputs.push(witness.output);
        target.push(witness.canon_output);
    }

    // all outputs identical → constant behavior is all the evidence shows;
    // the only place ConstOut is ever proposed
    if target.iter().all(|c| *c == target[0]) {
        return SearchOutcome::Found(Synthesis {
            program: Program::new(vec![Op::ConstOut {
                value: outputs[0].clone(),
            }]),
            distinct_inputs: inputs.len(),
            states_explored: 1,
            depth_reached: 1,
        });
    }

    search(&inputs, &outputs, &target, budget)
}

// ---- private search internals ----

/// One deduped observation, canonicalized once.
struct Witness {
    input: Value,
    output: Value,
    canon_output: String,
}

/// A frontier entry: registers across all witnesses plus the ops that
/// built them.
struct Candidate {
    regs: Vec<Value>,
    ops: Vec<Op>,
}

/// Breadth-first search from the input vector toward the output vector.
fn search(
    inputs: &[Value],
    outputs: &[Value],
    target: &[String],
    budget: SearchBudget,
) -> SearchOutcome {
    let distinct_inputs = inputs.len();
    let instances = static_instances(mine_field_keys(inputs));

    let root_canon: Vec<String> = inputs.iter().map(canonical).collect();
    let root_digest = state_digest(&root_canon);
    let mut seen = BTreeSet::from([root_digest]);
    let mut states_explored = 1_usize;
    let mut depth_reached = 0_usize;
    if states_explored >= budget.max_states {
        return SearchOutcome::BudgetExhausted {
            states_explored,
            depth_reached,
        };
    }

    let mut frontier = BTreeMap::from([(
        root_digest,
        Candidate {
            regs: inputs.to_vec(),
            ops: Vec::new(),
        },
    )]);

    for depth in 1..=budget.max_depth {
        let mut next: BTreeMap<[u8; 32], Candidate> = BTreeMap::new();
        for parent in frontier.values() {
            let gate = Gate::of(&parent.regs);
            let mined = mined_add(&parent.regs, outputs, &gate);
            for instance in instances
                .iter()
                .filter(|i| gate.admits(i.req))
                .chain(mined.iter())
            {
                let Some(regs) = apply(&instance.prog, &parent.regs) else {
                    continue; // errors on some witness: not part of any fit
                };
                let canon: Vec<String> = regs.iter().map(canonical).collect();
                let digest = state_digest(&canon);
                if canon == target {
                    // checked before the dedupe so a target equal to the
                    // start state (identity behavior) is still findable
                    if seen.insert(digest) {
                        states_explored += 1;
                    }
                    let mut ops = parent.ops.clone();
                    ops.push(instance.op.clone());
                    return SearchOutcome::Found(Synthesis {
                        program: Program::new(ops),
                        distinct_inputs,
                        states_explored,
                        depth_reached: depth,
                    });
                }
                if !seen.insert(digest) {
                    continue; // observationally equivalent to a known state
                }
                states_explored += 1;
                depth_reached = depth;
                if states_explored >= budget.max_states {
                    return SearchOutcome::BudgetExhausted {
                        states_explored,
                        depth_reached,
                    };
                }
                let mut ops = parent.ops.clone();
                ops.push(instance.op.clone());
                next.insert(digest, Candidate { regs, ops });
            }
        }
        if next.is_empty() {
            break; // reachable space closed below max_depth
        }
        frontier = next;
    }

    SearchOutcome::BudgetExhausted {
        states_explored,
        depth_reached,
    }
}

/// Register type a candidate op requires; used to skip instances whose
/// input type cannot match. Full eval remains the truth for everything
/// admitted.
#[derive(Clone, Copy)]
enum Req {
    Object,
    Text,
    List,
    TextList,
    Int,
}

/// Cheap per-state type facts: whether ALL registers satisfy a class.
struct Gate {
    object: bool,
    text: bool,
    list: bool,
    text_list: bool,
    int: bool,
}

impl Gate {
    fn of(regs: &[Value]) -> Self {
        let mut gate = Self {
            object: true,
            text: true,
            list: true,
            text_list: true,
            int: true,
        };
        for reg in regs {
            gate.object &= reg.is_object();
            gate.text &= reg.is_string();
            let (list, text_list) = match reg {
                Value::Array(items) => (true, items.iter().all(Value::is_string)),
                _ => (false, false),
            };
            gate.list &= list;
            gate.text_list &= text_list;
            gate.int &= reg.as_i64().is_some();
        }
        gate
    }

    fn admits(&self, req: Req) -> bool {
        match req {
            Req::Object => self.object,
            Req::Text => self.text,
            Req::List => self.list,
            Req::TextList => self.text_list,
            Req::Int => self.int,
        }
    }
}

/// One candidate op with its type gate and a prebuilt single-op program
/// (built once; per-application cost is eval only).
struct Instance {
    req: Req,
    op: Op,
    prog: Program,
}

impl Instance {
    fn new(req: Req, op: Op) -> Self {
        let prog = Program::new(vec![op.clone()]);
        Self { req, op, prog }
    }
}

/// `Add` deltas always proposed; a mined uniform delta may follow them.
const BASE_ADD_DELTAS: [i64; 4] = [-2, -1, 1, 2];

/// The v0 candidate space in its documented expansion order — [`Op`]
/// declaration order, parameters in listed order. This order is part of the
/// determinism contract: among equal-length programs the earliest
/// (parent-state digest, instance index) pair wins. `ConstOut` is absent by
/// design.
fn static_instances(field_keys: Vec<String>) -> Vec<Instance> {
    let mut out = Vec::new();
    for key in field_keys {
        out.push(Instance::new(Req::Object, Op::GetField { key }));
    }
    for op in [Op::Lowercase, Op::Uppercase, Op::Trim, Op::SplitWhitespace] {
        out.push(Instance::new(Req::Text, op));
    }
    for sep in [" ", ",", ", ", "-", ":"] {
        out.push(Instance::new(Req::Text, Op::SplitOn { sep: sep.into() }));
    }
    for set in [".", ",", ".,", ".,!?;:"] {
        out.push(Instance::new(
            Req::TextList,
            Op::TrimEachMatches { set: set.into() },
        ));
    }
    for n in 1..=8 {
        out.push(Instance::new(Req::TextList, Op::FilterLongerThan { n }));
    }
    out.push(Instance::new(Req::TextList, Op::DedupSort));
    for n in 1..=5 {
        out.push(Instance::new(Req::List, Op::Take { n }));
    }
    out.push(Instance::new(Req::List, Op::First));
    out.push(Instance::new(Req::List, Op::Last));
    for sep in ["", " ", ",", ", ", "-"] {
        out.push(Instance::new(Req::TextList, Op::Join { sep: sep.into() }));
    }
    out.push(Instance::new(Req::List, Op::Count));
    out.push(Instance::new(Req::Text, Op::CharCount));
    for k in BASE_ADD_DELTAS {
        out.push(Instance::new(Req::Int, Op::Add { k }));
    }
    out
}

/// `GetField` key candidates: top-level keys of every observed input
/// object, sorted and deduped.
fn mine_field_keys(inputs: &[Value]) -> Vec<String> {
    let mut keys = BTreeSet::new();
    for input in inputs {
        if let Value::Object(map) = input {
            keys.extend(map.keys().cloned());
        }
    }
    keys.into_iter().collect()
}

/// Mined `Add` delta: proposed only when every register and its target
/// output are ints differing by one uniform nonzero `k` outside
/// [`BASE_ADD_DELTAS`]. Expanded after the base deltas (documented order).
fn mined_add(regs: &[Value], outputs: &[Value], gate: &Gate) -> Option<Instance> {
    if !gate.int {
        return None;
    }
    let mut delta: Option<i64> = None;
    for (reg, out) in regs.iter().zip(outputs) {
        let k = out.as_i64()?.checked_sub(reg.as_i64()?)?;
        match delta {
            None => delta = Some(k),
            Some(d) if d != k => return None,
            Some(_) => {}
        }
    }
    let k = delta?;
    if k == 0 || BASE_ADD_DELTAS.contains(&k) {
        return None;
    }
    Some(Instance::new(Req::Int, Op::Add { k }))
}

/// Apply a single-op program to every register; `None` prunes the
/// candidate (an op that errors on any witnessed input cannot appear in a
/// fitting pipeline).
fn apply(prog: &Program, regs: &[Value]) -> Option<Vec<Value>> {
    regs.iter().map(|reg| eval(prog, reg).ok()).collect()
}

/// Canonical JSON: `serde_json::to_string` — object keys sorted
/// (`preserve_order` is off workspace-wide), number representation exact.
fn canonical(v: &Value) -> String {
    serde_json::to_string(v).expect("Value serialization cannot fail")
}

/// Observational-equivalence key: sha-256 over the canonical register
/// vector, each entry length-prefixed (unambiguous concatenation).
fn state_digest(canon_regs: &[String]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for canon in canon_regs {
        hasher.update((canon.len() as u64).to_le_bytes());
        hasher.update(canon.as_bytes());
    }
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;

    /// Test oracle: port of the python reference
    /// (`evals/toy-agent/agent.py`, `fake_model`) — lowercase,
    /// whitespace-split, strip `.,`, keep words longer than 4 chars, sorted
    /// dedupe, first three, space-join.
    fn fake_model(prompt: &str) -> String {
        let keywords: BTreeSet<String> = prompt
            .to_lowercase()
            .split_whitespace()
            .map(|w| w.trim_matches(|c| ".,".contains(c)).to_owned())
            .filter(|w| w.chars().count() > 4)
            .collect();
        keywords.into_iter().take(3).collect::<Vec<_>>().join(" ")
    }

    const DOC_A: &str = "The quick brown fox jumps over the lazy dog near the riverbank.";
    const DOC_B: &str = "Compilers translate agent cognition into fast deterministic binaries.";
    /// held out of synthesis wherever used — generalization evidence
    const DOC_C: &str = "Traces prove most model calls are secretly parsers.";

    fn fake_frontier_observations() -> Vec<Observation> {
        [DOC_A, DOC_B]
            .iter()
            .map(|doc| Observation {
                input: json!({ "prompt": doc }),
                output: json!(fake_model(doc)),
            })
            .collect()
    }

    fn expect_found(outcome: SearchOutcome) -> Synthesis {
        match outcome {
            SearchOutcome::Found(found) => found,
            other => panic!("expected Found, got {other:?}"),
        }
    }

    // (a) two witnesses pin the fake-frontier pipeline; doc C is held out
    #[test]
    fn fake_frontier_two_observations_synthesize_and_generalize() {
        // pin the oracle port to the recorded behavior (auto-dsl golden)
        assert_eq!(fake_model(DOC_A), "brown jumps quick");

        let started = Instant::now();
        let outcome = synthesize(&fake_frontier_observations(), SearchBudget::default());
        let elapsed = started.elapsed();

        let found = expect_found(outcome);
        assert_eq!(found.distinct_inputs, 2);

        // memo-tables are inexpressible: ConstOut exists only on the
        // depth-1 all-identical path, never inside a searched pipeline
        assert!(
            !found
                .program
                .ops
                .iter()
                .any(|op| matches!(op, Op::ConstOut { .. })),
            "searched pipeline must not contain ConstOut: {:?}",
            found.program
        );

        // generalization, not memorization: doc C was never observed
        assert_eq!(
            eval(&found.program, &json!({ "prompt": DOC_C })).expect("program evals on held-out"),
            json!(fake_model(DOC_C)),
        );

        // measured wall time (visible under `cargo test -- --nocapture`)
        println!(
            "fake-frontier synthesis: {elapsed:?} ({} states, depth {})",
            found.states_explored, found.depth_reached
        );
        assert!(
            elapsed < Duration::from_secs(60),
            "took {elapsed:?}; must finish well under 60s in debug"
        );
    }

    // (b) wordcount from two witnesses, held-out single-word input
    #[test]
    fn wordcount_synthesizes_and_generalizes() {
        let obs = vec![
            Observation {
                input: json!({ "text": "a b c" }),
                output: json!(3),
            },
            Observation {
                input: json!({ "text": "one two" }),
                output: json!(2),
            },
        ];
        let found = expect_found(synthesize(&obs, SearchBudget::default()));
        // shortest fit, ties by documented order
        assert_eq!(
            found.program.ops,
            vec![
                Op::GetField { key: "text".into() },
                Op::SplitWhitespace,
                Op::Count
            ]
        );
        assert_eq!(
            eval(&found.program, &json!({ "text": "x" })).expect("program evals on held-out"),
            json!(1)
        );
    }

    // (c) identical outputs across different inputs → depth-1 ConstOut
    #[test]
    fn identical_outputs_yield_depth_one_const_out() {
        let obs = vec![
            Observation {
                input: json!({ "text": "a b c" }),
                output: json!("ok"),
            },
            Observation {
                input: json!({ "text": "entirely different" }),
                output: json!("ok"),
            },
        ];
        let found = expect_found(synthesize(&obs, SearchBudget::default()));
        assert_eq!(found.program.ops, vec![Op::ConstOut { value: json!("ok") }]);
        assert_eq!(found.distinct_inputs, 2);
        assert_eq!(found.states_explored, 1);
        assert_eq!(found.depth_reached, 1);
    }

    // (d) same canonical input, different outputs → conflict
    #[test]
    fn conflicting_observations_are_rejected() {
        // key order differs but canonical form (sorted keys) is the same
        // input, so the differing outputs are a genuine conflict
        let obs = vec![
            Observation {
                input: json!({ "a": 1, "b": 2 }),
                output: json!("x"),
            },
            Observation {
                input: json!({ "b": 2, "a": 1 }),
                output: json!("y"),
            },
        ];
        assert_eq!(
            synthesize(&obs, SearchBudget::default()),
            SearchOutcome::ConflictingObservations
        );
    }

    // (e) the documented honesty boundary: with one distinct input (one
    // witness) constant behavior is indistinguishable from computation, so
    // the outcome is the ConstOut program — nothing more is claimable
    #[test]
    fn one_distinct_input_hits_the_documented_honesty_boundary_const_out() {
        // two records, one canonical input: dedupe leaves a single witness
        let obs = vec![
            Observation {
                input: json!({ "text": "a b c" }),
                output: json!(3),
            },
            Observation {
                input: json!({ "text": "a b c" }),
                output: json!(3),
            },
        ];
        let found = expect_found(synthesize(&obs, SearchBudget::default()));
        assert_eq!(found.distinct_inputs, 1);
        assert_eq!(found.program.ops, vec![Op::ConstOut { value: json!(3) }]);
        assert_eq!(found.depth_reached, 1);
    }

    // (f) unreachable target under a tiny budget → honest exhaustion
    #[test]
    fn unreachable_target_under_tiny_budget_exhausts_honestly() {
        let budget = SearchBudget {
            max_depth: 2,
            max_states: 50,
        };
        let outcome = synthesize(&fake_frontier_observations(), budget);
        let (states_explored, depth_reached) = match outcome {
            SearchOutcome::BudgetExhausted {
                states_explored,
                depth_reached,
            } => (states_explored, depth_reached),
            other => panic!("expected BudgetExhausted, got {other:?}"),
        };
        // plausible counters: root plus at least the get_field state,
        // capped by the budget
        assert!(
            (2..=50).contains(&states_explored),
            "states_explored={states_explored}"
        );
        assert!(
            (1..=2).contains(&depth_reached),
            "depth_reached={depth_reached}"
        );
    }

    // (g) determinism: same observations, same program — in any order
    #[test]
    fn determinism_same_observations_always_yield_the_same_program() {
        let obs = fake_frontier_observations();
        let first = synthesize(&obs, SearchBudget::default());
        let second = synthesize(&obs, SearchBudget::default());
        assert_eq!(first, second);

        // observation order is canonicalized away too
        let mut reversed = obs.clone();
        reversed.reverse();
        assert_eq!(synthesize(&reversed, SearchBudget::default()), first);
    }

    // mined Add delta: base deltas cannot bridge a uniform offset of 7
    #[test]
    fn mined_add_delta_bridges_uniform_integer_offsets() {
        let obs = vec![
            Observation {
                input: json!({ "x": "ab" }),
                output: json!(9),
            },
            Observation {
                input: json!({ "x": "abcde" }),
                output: json!(12),
            },
        ];
        let found = expect_found(synthesize(&obs, SearchBudget::default()));
        assert_eq!(
            found.program.ops,
            vec![
                Op::GetField { key: "x".into() },
                Op::CharCount,
                Op::Add { k: 7 }
            ]
        );
        assert_eq!(
            eval(&found.program, &json!({ "x": "z" })).expect("program evals on held-out"),
            json!(8)
        );
    }

    // no observations: nothing witnessed, nothing searched
    #[test]
    fn no_observations_exhaust_at_zero() {
        assert_eq!(
            synthesize(&[], SearchBudget::default()),
            SearchOutcome::BudgetExhausted {
                states_explored: 0,
                depth_reached: 0
            }
        );
    }
}
