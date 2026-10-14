## What and why

<!-- Briefly: what changes and which roadmap milestone / task it closes. -->

## Checklist

- [ ] `cargo fmt`, `clippy -D warnings`, `cargo test` pass locally
- [ ] `python tools/audit_workspace.py .` is clean
- [ ] New public items are documented, `lib.rs` has a doctest
- [ ] Tests: unit / property / loom (for atomic code)
- [ ] `unsafe` only in `pragma_sync` / `pragma_sys` (`layers.toml`), every block with `// SAFETY:`
- [ ] A new dependency or exception is recorded in `DECISIONS.md`
- [ ] `CHANGELOG.md` is updated (for notable changes)
