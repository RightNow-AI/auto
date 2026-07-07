# fake_frontier_impl

The hand-assisted S3 implementation of the toy agent's `fake-frontier` span —
the wasm module that goes inside the first `.cbin`. The S3 spine item allows
hand-assisted passes, so a human wrote this crate; automated symbolic
extraction lands S4. ABI: spec/artifact.md §4 (zero imports; exports
`memory`, `alloc`, `run`). The crate is deliberately outside the repo
workspace (its own `[workspace]` root): host builds and lockfiles stay
separate from the wasm target.

Build the module:

    cargo build --release --target wasm32-unknown-unknown --manifest-path evals/toy-agent/fake-frontier-impl/Cargo.toml

The artifact lands at
`evals/toy-agent/fake-frontier-impl/target/wasm32-unknown-unknown/release/fake_frontier_impl.wasm`.

Unit-test the mapping natively (host target, no wasm involved):

    cargo test --manifest-path evals/toy-agent/fake-frontier-impl/Cargo.toml

## feature `wrong`

`--features wrong` builds a deliberately divergent variant (2 keywords
instead of 3). The e2e compiles it to prove the emit gate **blocks** an
implementation whose outputs differ from the recorded observations. Never
ship it. Both variants land at the **same output path** — rebuild without
the feature to restore the correct module.

## failure mode

Input without a string `"prompt"` key panics, which traps at the ABI; the
host reports the trap as an execution failure. Honest and strict — the
interface input type is `json`, so such input is reachable, and this module
never invents an output for it.
