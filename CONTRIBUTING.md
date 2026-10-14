# Contributing

## Before a PR

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
python tools/audit_workspace.py .
```

If `pragma_sync` changed, also:

```bash
cargo +nightly miri test -p pragma_sync
RUSTFLAGS="--cfg loom" cargo test -p pragma_sync --release --lib --tests
```

If `pragma_jobs` changed: the long stress tests (nightly in CI) and the benches against `rayon`:

```bash
cargo test -p pragma_jobs --release --test stress -- --ignored
cargo bench -p pragma_jobs
```

If `pragma_graph` changed: the frame bench and the builder fuzzer (cargo-fuzz, nightly, Linux;
a separate `fuzz/` workspace). The "allocation-free frame" test is part of the regular `cargo test`.

```bash
cargo bench -p pragma_graph
cargo +nightly fuzz run graph_build -- -max_total_time=60
```

## Module contract

A new crate or module is considered done when it has:

1. **A place in a layer**: an entry in `layers.toml`, dependencies only on lower layers.
2. **A typed error**: a `#[non_exhaustive]` enum via `thiserror`, no `unwrap`/`expect`/`panic!`/indexing.
3. **Tests**: unit tests and property tests for invariants; a loom model for an atomic protocol.
4. **A feature flag** in the `pragma_core` facade, if the module is optional.
5. **Documentation**: `//!` in `lib.rs` with a doctest example; `missing_docs` is clean.
6. **A benchmark** (`criterion`), if the module is on a hot path.

## Dependencies

- Pure Rust only (`deny.toml`). Before adding one, run `cargo tree -e build -i cc` and the audit.
- The version goes only into `[workspace.dependencies]`; in the crate, `{ workspace = true }`.
- A new dependency or an exception to the rules means a row in `DECISIONS.md`.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/): `feat(jobs): ...`, `fix(sync): ...`,
`docs: ...`, `ci: ...`, `refactor(graph): ...`. A breaking change is `feat(jobs)!: ...` plus an entry in `CHANGELOG.md`.
