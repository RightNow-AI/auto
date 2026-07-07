# Auto IR — specification, v0

Status: v0, matches `crates/auto-ir` as merged. Schema version: **0**.
Normative schema: `crates/auto-ir/schema/ir.fbs`. This document is written for
external readers; where prose and schema disagree, the schema plus the
validation rules below win.

The Auto IR is a typed task graph: the exchange format between trace capture,
compiler passes, verification, and the runtime. Nodes carry their **effects**
(what they may touch), their **uncertainty class** (how much of their behavior
is fixed by their inputs), and their **resource bounds** (declared ceilings).
Regions group nodes for extraction. The IR is data, not code: v0 defines
structure and well-formedness, not an execution semantics (that arrives with
contracts and the backend, S2/S3).

## 1. identity

| id | width | scope | stability |
|---|---|---|---|
| `GraphId` | u128 | global | minted by the producer, never by the IR; uuid-shaped values recommended |
| `NodeId` | u64 | one graph | never reused within a graph; never renumbered by any pass; preserved bit-exactly by serialization |
| `RegionId` | u64 | one graph | same rules as `NodeId`; separate namespace from `NodeId` |

Ids are opaque. No ordering, density, or contiguity may be assumed beyond
uniqueness. On the wire, `GraphId` is stored as two u64 fields (`id_hi`,
`id_lo`), composed as `(id_hi << 64) | id_lo`.

## 2. structure

A **graph** is: `schema_version` (u32, wire-level), `id` (GraphId), `name`
(utf-8, may be empty), a set of **nodes**, a set of **edges**, and a set of
**regions**.

A **node** is: `id`, `name` (may be empty), a **kind**, ordered typed
**input ports**, ordered typed **output ports**, a set of **capability
effects**, a set of **memory effects**, an **uncertainty class**, and
**resource bounds**.

A **port** is a `name` (may be empty) plus a **value type**. Ports are
addressed by index; port order is semantic. Port names are display-only.

An **edge** is `(from_node, from_port) → (to_node, to_port)`: dataflow from an
output port of the source node to an input port of the destination node.
Edges have no identity beyond these four fields; duplicate edges are invalid.

A **region** is `id`, `name`, and a non-empty set of member node ids — the
unit of extraction for later passes. v0 regions are flat (no nesting) and may
overlap.

## 3. type system

Value types flowing along edges:

```
unit | bool | int | float | text | bytes | json | list<T>
```

- `unit` — no payload; pure sequencing.
- `int` — 64-bit signed integer semantics. `float` — IEEE-754 binary64.
- `text` — utf-8. `bytes` — opaque byte string.
- `json` — structured value with no schema commitment; the v0 escape hatch.
- `list<T>` — homogeneous; element type is any value type, including nested
  lists.

The single typing rule: **an edge's source output port type must equal its
destination input port type** (exact structural equality). There is no
subtyping and no coercion in v0.

## 4. node kinds and arity

| kind | payload | inputs | outputs | meaning |
|---|---|---|---|---|
| `input` | — | exactly 0 | exactly 1 | graph entry |
| `output` | — | exactly 1 | exactly 0 | graph exit |
| `tool_call` | `tool` (utf-8 name) | any | any | invoke an external tool |
| `model_call` | `model_class` (utf-8 hint) | any | any | invoke a model; `model_class` is a routing hint ("frontier", "distilled-0.5b"), never a capability or parity claim |
| `transform` | `op` (utf-8 description) | any | any | computation with no effects beyond its declared sets |
| `branch` | — | ≥ 1 | ≥ 2 | decision point; exactly one out-branch carries a value at runtime |

Branch selection semantics (which arm fires, and how the predicate is
represented) are intentionally unspecified in v0 — see
`spec/adr/open-questions.md`. A graph may have any number of `input` and
`output` nodes, including zero.

## 5. effect semantics

Effects are **declarations carried by the IR**. At S0 nothing enforces them at
runtime; from S3 on, the backend confines the compiled artifact to exactly the
declared capability set at the wasm/wasi boundary. The declared set is a
ceiling, not a promise of use: a node declaring `net` may make no request, but
a node exercising an undeclared capability is a compile-time validation
failure and, once artifacts exist, a runtime trap. Never a silent grant.

Capability effects (wire values in parentheses):

- `net` (0) — open sockets, make requests
- `fs` (1) — touch the filesystem
- `exec` (2) — spawn processes / execute code outside the graph
- `secrets` (3) — read secret material (api keys, credentials)
- `payments` (4) — move money

Memory effects target the **task's memory store** — the agent-visible
key/value+log state, which is not the filesystem (that is `fs`):

- `read` (0) — consult the store
- `write` (1) — overwrite a key
- `append` (2) — append to a log-shaped key

Both effect fields are sets: duplicates are invalid on the wire, and element
order is not semantic. An empty capability set means fully confined; an empty
memory set means memory-silent.

## 6. uncertainty classes

Per node (wire values in parentheses):

- `deterministic` (0) — same inputs → same outputs, always. The candidate set
  for symbolic extraction.
- `probabilistic` (1) — the output distribution is narrow and checkable:
  retries, low-temperature sampling, flaky externals.
- `generative` (2) — open-ended production; outputs are judged by contract,
  not by equality.

v0 does not couple uncertainty to node kind (a `tool_call` may be generative —
it may invoke another model; a `model_call` result cached to a fixed answer
may be deterministic). Classification accuracy is measured, not asserted:
S1's determinism report exists to check these labels against traces.

## 7. resource bounds

Per node, all optional u64s: `max_latency_ms`, `max_cost_usd_micros`
(micro-usd: 1,000,000 = $1), `max_tokens`, `max_memory_bytes`.

`null`/absent means **not declared**. A bound is a declared ceiling supplied
by the producer — never a measured number, and never fabricated. Measured
performance lives in the artifact manifest (S7), attached to eval run ids.

## 8. invariants

A graph is valid iff all of the following hold (`Graph::validate`, run by
both serialize and deserialize — an invalid graph can neither be emitted nor
loaded):

1. Node ids unique; region ids unique (map key equals embedded id).
2. Port counts addressable by u32.
3. Kind arity rules (§4) hold.
4. Every edge references existing nodes and in-range ports.
5. Edge typing rule (§3) holds.
6. **Single assignment:** every input port of every node is driven by exactly
   one edge — no dangling inputs, no fan-in. (Output ports may fan out to any
   number of consumers, or none.)
7. The graph is **acyclic**. v0 has no loops; iteration is an open question.
8. Regions are non-empty and reference existing nodes.
9. No duplicate edges.

The empty graph (no nodes, no edges, no regions) is valid.

## 9. serialization

Format: [flatbuffers], root table `Graph`, file identifier **`AIR0`** at
bytes 4..8. Conventional file extension: `.air`. Rationale and alternatives:
`spec/adr/0001-ir-serialization.md`.

Wire enums: `TypeKind` `unit=0 bool=1 int=2 float=3 text=4 bytes=5 json=6
list=7` (a `list` TypeRef carries `elem`; any other kind must not).
`NodeKind` union tags: `InputK=1 OutputK=2 ToolCallK=3 ModelCallK=4
TransformK=5 BranchK=6`.

### canonical encoding

`to_bytes` validates, then writes nodes, edges, regions, effect sets, and
region members in ascending order (by id; edges by `(from, from_port, to,
to_port)`), with a fixed field-construction order. The flatbuffers builder is
deterministic given a deterministic write sequence, so:

- **equal graphs serialize to identical bytes**, regardless of construction
  order;
- **round-trip is byte-stable:** for any valid graph `g`,
  `from_bytes(to_bytes(g)) == g` and `to_bytes(from_bytes(to_bytes(g))) ==
  to_bytes(g)`, byte for byte. The golden files under
  `crates/auto-ir/tests/golden/` pin this encoding; a byte drift there is a
  wire-format change and requires a schema-version decision.

### decode strictness

`from_bytes` rejects, in order: buffers shorter than 8 bytes or without the
`AIR0` identifier; anything failing the flatbuffers verifier (run with
default limits: nesting depth 64, table budget 1,000,000, apparent size 2³¹ —
deeply nested `list<...>` types beyond ~60 levels are therefore unreadable);
`schema_version` ≠ 0; unknown enum values (capability, memory, uncertainty,
type kind, node kind); malformed `TypeRef`s; duplicate node ids, duplicate
edges, duplicate region ids, duplicate effect entries, duplicate region
members; and finally anything failing the §8 invariants.

Element **order** of the node/edge/region vectors and effect sets is not
semantic: a foreign buffer with valid but non-canonically-ordered vectors
decodes successfully, and re-serialization canonicalizes it. Port order IS
semantic and is preserved exactly. (Scalar-field presence-vs-default is not
observable through this implementation and is likewise canonicalized on
re-write; see open-questions.)

## 10. versioning

The wire format carries an explicit `schema_version` (u32) in the root table;
this build writes and reads **exactly version 0**. Policy:

- Any change that alters the meaning or the canonical bytes of existing valid
  graphs bumps `schema_version` and re-blesses goldens in the same change,
  with an ADR.
- v0 readers reject any other version loudly (`UnsupportedSchemaVersion`) —
  no silent best-effort reads.
- Backward-compatible read paths (reading version n−1) are a deliberate
  future decision, not an accident of flatbuffers field-tolerance; until one
  is specified, exact match is the rule.
- Flatbuffers schema evolution rules (append-only fields, never reuse ids)
  are followed *in addition to*, not instead of, the version field.

## 11. rendering

`auto inspect` prints a deterministic human-readable rendering (pinned by the
golden `.txt` files). It is a debugging surface, **not a stable machine
format** — parse the flatbuffers, not the text.

[flatbuffers]: https://flatbuffers.dev/
