//! Verification-gated artifact emission.
//!
//! The constitution rule "a failing contract blocks emit, no exceptions"
//! made mechanical: [`emit`] writes container bytes only for a `Pass`
//! verdict. Fail AND Inconclusive both refuse — Inconclusive means a
//! normative claim went unchecked, and unchecked is never rounded up to
//! emitted. The gating report must cite the same contract the manifest
//! claims, and v0 artifacts are pure: declared capabilities must be empty
//! (the runtime separately refuses modules with imports).

use std::collections::BTreeMap;

use auto_contract::harness::{Verdict, VerificationReport};

use crate::container::{
    Artifact, GRAPH_ENTRY, GUARD_ENTRY, MANIFEST_ENTRY, MODULE_ENTRY, PROGRAM_ENTRY,
};
use crate::manifest::Manifest;

/// Everything an emit needs besides the gate itself.
pub struct EmitInputs {
    pub manifest: Manifest,
    /// wasm module bytes (`module.wasm`)
    pub module: Vec<u8>,
    /// lowered IR of the compiled unit (`graph.air`), when available
    pub graph_air: Option<Vec<u8>>,
    /// synthesized DSL program (`program.json`) the embedded interpreter
    /// loads via the `init` ABI extension; None for hand-supplied modules
    pub program: Option<Vec<u8>>,
    /// runtime guard (`guard.json`); None = unguarded artifact
    pub guard: Option<Vec<u8>>,
}

/// Why an emit was refused. Every variant is a refusal; there is no
/// force flag.
#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error(
        "refusing to emit: gating verdict is {verdict}, not PASS \
         (a failing or inconclusive contract blocks emit, no exceptions)"
    )]
    VerdictNotPass { verdict: String },
    #[error(
        "refusing to emit: manifest cites contract {manifest} but the \
         gating report verified contract {gating}"
    )]
    ContractMismatch { manifest: String, gating: String },
    #[error(
        "refusing to emit: manifest capabilities must be sorted and unique          (the loader-enforced allowlist is canonical on the wire); declared          {declared:?}"
    )]
    CapabilitiesUnsorted { declared: Vec<String> },
}

/// Assemble and serialize a `.cbin` artifact, gated on `gating`. Returns the
/// canonical container bytes; their sha-256 is the artifact id.
pub fn emit(inputs: EmitInputs, gating: &VerificationReport) -> Result<Vec<u8>, EmitError> {
    if gating.verdict != Verdict::Pass {
        return Err(EmitError::VerdictNotPass {
            verdict: gating.verdict.to_string(),
        });
    }
    if inputs.manifest.contract_id != gating.contract_id {
        return Err(EmitError::ContractMismatch {
            manifest: inputs.manifest.contract_id.clone(),
            gating: gating.contract_id.clone(),
        });
    }
    // capability artifacts are real as of ADR-0017; the manifest list is
    // the loader-enforced allowlist and must be canonical (sorted, unique).
    // Import-level validation happens at load, where wasmtime can see the
    // module; the emit gate itself already executed the module differentially
    // through the same loader rules.
    let mut canonical_caps = inputs.manifest.capabilities.clone();
    canonical_caps.sort();
    canonical_caps.dedup();
    if canonical_caps != inputs.manifest.capabilities {
        return Err(EmitError::CapabilitiesUnsorted {
            declared: inputs.manifest.capabilities.clone(),
        });
    }

    let mut entries = BTreeMap::new();
    entries.insert(
        MANIFEST_ENTRY.to_owned(),
        inputs.manifest.canonical_json().into_bytes(),
    );
    entries.insert(MODULE_ENTRY.to_owned(), inputs.module);
    if let Some(graph) = inputs.graph_air {
        entries.insert(GRAPH_ENTRY.to_owned(), graph);
    }
    if let Some(program) = inputs.program {
        entries.insert(PROGRAM_ENTRY.to_owned(), program);
    }
    if let Some(guard) = inputs.guard {
        entries.insert(GUARD_ENTRY.to_owned(), guard);
    }
    Ok(Artifact::new(entries).to_bytes())
}

#[cfg(test)]
mod tests {
    use crate::manifest::{MANIFEST_VERSION, Measured, Provenance};

    use super::*;

    fn manifest(contract_id: &str) -> Manifest {
        Manifest {
            manifest_version: MANIFEST_VERSION,
            task: "t".into(),
            scope_kind: "model_call".into(),
            scope_name: "m".into(),
            interface_input: "json".into(),
            interface_output: "text".into(),
            capabilities: vec![],
            contract_id: contract_id.into(),
            eval_run_ids: vec!["run-1".into()],
            provenance: Provenance {
                trace_ids: vec!["0".repeat(32)],
                reference: "test reference".into(),
                observations: 2,
            },
            measured: Measured {
                compiled_latency_ms_p50: 1,
                compiled_latency_ms_p95: 2,
                compiled_latency_ms_max: 3,
                reference_recorded_latency_ms_p95: 40,
            },
            notes: String::new(),
        }
    }

    fn report(verdict: Verdict, contract_id: &str) -> VerificationReport {
        VerificationReport {
            contract_id: contract_id.into(),
            task: "t".into(),
            subject: "test subject".into(),
            verdict,
            observations: 2,
            checks: vec![],
        }
    }

    fn inputs(contract_id: &str) -> EmitInputs {
        EmitInputs {
            manifest: manifest(contract_id),
            module: b"\0asm-not-really".to_vec(),
            graph_air: Some(b"graph-bytes".to_vec()),
            program: None,
            guard: None,
        }
    }

    #[test]
    fn guard_entry_is_carried_when_present() {
        let mut with_guard = inputs("c1");
        with_guard.guard = Some(b"{\"guard_version\":0}".to_vec());
        let bytes = emit(with_guard, &report(Verdict::Pass, "c1")).expect("emit");
        let artifact = Artifact::from_bytes(&bytes).expect("parse");
        assert!(artifact.entries.contains_key(GUARD_ENTRY));
    }

    #[test]
    fn program_entry_is_carried_when_present() {
        let mut with_program = inputs("c1");
        with_program.program = Some(b"{\"dsl_version\":0,\"ops\":[\"trim\"]}".to_vec());
        let bytes = emit(with_program, &report(Verdict::Pass, "c1")).expect("emit");
        let artifact = Artifact::from_bytes(&bytes).expect("parse");
        assert_eq!(
            artifact.entries.get(PROGRAM_ENTRY).map(Vec::as_slice),
            Some(b"{\"dsl_version\":0,\"ops\":[\"trim\"]}".as_slice())
        );
    }

    #[test]
    fn refuses_fail() {
        let err = emit(inputs("c1"), &report(Verdict::Fail, "c1")).unwrap_err();
        match err {
            EmitError::VerdictNotPass { verdict } => assert_eq!(verdict, "FAIL"),
            other => panic!("wrong refusal: {other}"),
        }
    }

    /// The load-bearing gate: Inconclusive is NOT Pass. Nothing was violated,
    /// but something normative went unchecked — emit still refuses.
    #[test]
    fn refuses_inconclusive_load_bearing() {
        let err = emit(inputs("c1"), &report(Verdict::Inconclusive, "c1")).unwrap_err();
        match err {
            EmitError::VerdictNotPass { verdict } => assert_eq!(verdict, "INCONCLUSIVE"),
            other => panic!("wrong refusal: {other}"),
        }
    }

    #[test]
    fn refuses_contract_mismatch() {
        let err = emit(inputs("c1"), &report(Verdict::Pass, "c2")).unwrap_err();
        match err {
            EmitError::ContractMismatch { manifest, gating } => {
                assert_eq!(manifest, "c1");
                assert_eq!(gating, "c2");
            }
            other => panic!("wrong refusal: {other}"),
        }
    }

    #[test]
    fn refuses_unsorted_capabilities() {
        // capability artifacts are real as of ADR-0017 - a sorted, unique
        // list emits; a non-canonical list refuses
        let mut ok = inputs("c1");
        ok.manifest.capabilities = vec!["lookup".into()];
        emit(ok, &report(Verdict::Pass, "c1")).expect("sorted unique capabilities emit");

        for bad in [vec!["net", "fs"], vec!["net", "net"]] {
            let mut inputs = inputs("c1");
            inputs.manifest.capabilities = bad.iter().map(|s| s.to_string()).collect();
            let err = emit(inputs, &report(Verdict::Pass, "c1")).unwrap_err();
            assert!(
                matches!(err, EmitError::CapabilitiesUnsorted { .. }),
                "wrong refusal for {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn pass_emits_and_round_trips() {
        let gating = report(Verdict::Pass, "c1");
        let bytes = emit(inputs("c1"), &gating).expect("pass emits");
        let artifact = Artifact::from_bytes(&bytes).expect("emitted bytes parse");
        assert_eq!(
            artifact.manifest().expect("manifest parses"),
            manifest("c1")
        );
        assert_eq!(
            artifact.module_bytes().expect("module present"),
            b"\0asm-not-really"
        );
        assert_eq!(
            artifact.entries.get(GRAPH_ENTRY).map(Vec::as_slice),
            Some(b"graph-bytes".as_slice())
        );

        // identical inputs emit identical bytes: the artifact id is stable
        let bytes_again = emit(inputs("c1"), &gating).expect("pass emits");
        assert_eq!(bytes, bytes_again);
        let again = Artifact::from_bytes(&bytes_again).expect("parses");
        assert_eq!(artifact.id(), again.id());
    }

    #[test]
    fn graph_entry_is_optional() {
        let mut no_graph = inputs("c1");
        no_graph.graph_air = None;
        let bytes = emit(no_graph, &report(Verdict::Pass, "c1")).expect("pass emits");
        let artifact = Artifact::from_bytes(&bytes).expect("parses");
        assert!(!artifact.entries.contains_key(GRAPH_ENTRY));
    }
}
