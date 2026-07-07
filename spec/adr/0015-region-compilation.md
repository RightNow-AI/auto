# ADR-0015: region compilation — chains as pipelines, glue as synthesis, pure in v0

status: accepted · scope: `crates/auto-dsl` (`Pipeline`/`Payload`), `crates/auto-backend` (`gather_region`), `crates/auto-passes` (`region`), `crates/auto-contract` (region scope), `crates/auto-cli` (region compile, N-node lowering), `spec/synthesis.md` §8, `spec/contract.md`

## context

Every artifact so far compiles ONE span. The constitution's compiler
compiles agents — multi-span behavior with the effect system enforcing at
the artifact boundary. The gap between one span and a whole agent is the
**region**: a chain of spans plus the agent code between them. That
between-code (the glue) is never recorded as code; it IS recorded as
values — every trace witnesses (span k output, span k+1 input) pairs. So
glue is a synthesis problem with exactly the same evidence discipline as
everything else.

## decision

1. **A region is a declared chain**, contract scope
   `region { from, to }`: the effectful spans from `from` through `to` by
   seq order. Structure rules are loud — one `from`/`to` per trace, unique
   names in the window, identical (kind, name) sequence across every
   recorded trace. Ambiguity refuses; nothing is inferred.
2. **Every arrow is a synthesis problem.** Stages (span input → output)
   and glue edges (output → next input) each synthesize independently over
   their witnessed pairs, each with the full enumerative budget. One
   unsynthesizable edge refuses the whole region, naming the edge — a
   region is not a partial promise.
3. **Identity glue is omitted, not expressed.** The DSL deliberately has
   no identity program (a program must transform); glue whose witnessed
   pairs are all equal simply does not appear in the pipeline. The
   assembled payload is `{"pipeline_version":0,"programs":[…]}` —
   `auto_dsl::Pipeline`, strict-parsed, evaluated by folding the existing
   evaluator; the interpreter accepts program or pipeline payloads by
   version-key sniff. One implementation, two compilations, unchanged.
4. **v0 regions are pure: `model_call` chains only.** A tool_call (or any
   other effectful kind) inside the window refuses. Compiling tool-calling
   regions is THE next step — artifacts with *declared capability imports*
   that the loader admits selectively, turning the manifest's capability
   list from "empty by construction" into an enforced allowlist. That is a
   loader + ABI + manifest change with its own ADR; claiming it early
   would fake the constitution's central confinement promise.
5. **The gate is unchanged.** Differential replay covers every recorded
   end-to-end chain; examples/properties run against the assembled
   pipeline; the guard calibrates on from-span inputs; `graph.air` lowers
   one transform node per stage (the chain made visible in the IR).
   Region deopt answers are returned but not ingested in v0 (a region
   witness is a chain; one end-to-end pair has no per-stage attribution) —
   stated at the deopt site, recorded here.

## alternatives considered

**Whole-task compilation first.** The constitution's endpoint, but traces
carry no task-level I/O (recorded open question since S2), and a task is
just the maximal region — regions are the mechanism task compilation will
reuse once the trace format grows task I/O.

**Recording the glue as code (tracing the agent's interpreter).** Would
capture glue exactly, but requires language-level instrumentation per SDK
runtime and abandons the trace model's I/O-only honesty. Glue-as-synthesis
keeps the evidence discipline: what was witnessed is what is claimed.

**One synthesized program over (from-input → to-output) directly.** Loses
the chain: no per-stage provenance, no per-stage IR nodes, no path to
swapping one stage for a distilled model or a capability import later. The
end-to-end function is also usually deeper than the per-edge functions —
per-edge search is where enumerative synthesis stays tractable.

**Allowing tool calls now via host re-execution.** Tempting (the runtime
could perform the recorded tool call live), but it silently converts a
"compiled artifact" into "an orchestrator with network access" without the
capability machinery to confine it. Refusing is honest; the import
machinery is the recorded upgrade.

## consequences

- The compiler now compiles CHAINS — the first structural step from
  "compiles functions" toward "compiles agents". `graph.air` grows real
  multi-node structure.
- Two payload forms exist behind one sniffing parser; artifact consumers
  need no changes (the interpreter change shipped inside the module).
- Region synthesis cost is per-edge × budget; deep chains multiply search
  time linearly. Stage-level LLM-CEGIS and per-stage distillation are
  natural extensions (recorded, not built).
- Trace-mode `auto verify` refuses region contracts (the harness gathers
  spans, not chains, in trace mode) — the emit gate is the verification
  story for v0 regions, and the refusal says so.
