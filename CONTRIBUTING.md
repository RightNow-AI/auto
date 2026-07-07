# Contributing

Thanks for looking at Auto. `CLAUDE.md` records the engineering norms we held to;
read it before a first change. This file is the short version of the norms
that gate a merge.

## Build gates

CI runs these on every pull request, and they must be green before merge:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`clippy -D warnings` means warnings fail the build. No `unsafe` without a
justification comment above it (the workspace denies `unsafe_code`).

Toolchain is pinned: rust 1.96.1 (`rust-toolchain.toml`) and `flatc` 25.12.19
exactly (it must match the `flatbuffers` crate; the IR build fails on a
mismatch). See the README build section for how the build resolves `flatc`.

## End-to-end loops

Changes to the pipeline should keep the end-to-end scripts green. They record,
compile, run, and prove the negative paths (emit blocked on a wrong module,
guard abstention, deopt and recompile, registry tamper refused):

```
cargo build -p auto-cli
bash evals/toy-agent/e2e.sh
PYTHON_BIN=python bash evals/distill-agent/e2e.sh
```

CI runs the full set (serve-proxy, pipeline-agent, daemon, tool-agent,
registry-remote) in `.github/workflows/ci.yml`. None of them touch the
network, and none spend money.

## No unverified claims

Honesty is load-bearing here.

- Manifests report measured numbers or `null`. Never fabricated, never rounded
  up.
- A parity or performance claim carries an eval-run id. A failing or
  unmeasurable contract blocks emit; that is the point of the gate.
- No pass merges without differential tests against the reference interpreter.
- A stub is labeled `stub` in code, in `--help`, and in docs. No mock pretends
  to be the compiler.

## Decisions and scope

- Irreversible decisions get a numbered, terse ADR under `spec/adr/`, with
  alternatives listed. Anything under `spec/` is written for external readers.
- Adjacent gaps you notice but do not fix belong in
  `spec/adr/open-questions.md`, not in scope creep.

## Commits and pull requests

- Granular commits with honest messages. One logical change per commit.
- Never push to `main` directly. Open a pull request; keep CI green.
- Frontier API spend has a hard per-session cap and is logged to a ledger.
  We built this under limited resources, so paid OpenAI usage stays capped.

By contributing you agree that your contributions are licensed under the
Apache License 2.0 (see `LICENSE`).
