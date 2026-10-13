# pragma.core

[![CI](https://github.com/razedproduction/pragma.core/actions/workflows/ci.yml/badge.svg)](https://github.com/razedproduction/pragma.core/actions/workflows/ci.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.88-orange)
![License](https://img.shields.io/badge/license-MIT-blue)
![Status](https://img.shields.io/badge/status-pre--alpha-red)

**pragma.core** is a pure-Rust, solid and high-performance multithreaded core for
razed.production's game engines and frameworks.

> **Русский:** pragma.core — надёжное и высокопроизводительное многопоточное ядро на
> чистом Rust для игровых движков и фреймворков. Документация проекта — в [`docs/`](docs/README.md).

It is the layer *under* an engine: it owns the threads, schedules the work of a frame
and runs background I/O, so that ECS, rendering, physics and audio can be written on top
of it without each inventing their own thread pool.

## Status

Pre-alpha. The workspace, CI, L0/L1 primitives and the work-stealing job system (M1) are in
place; the per-frame task graph is the next milestone. See the [roadmap](docs/roadmap.md).

## Features (planned)

| Area | Crate | Status |
|---|---|---|
| Errors, generational handles, simulation ticks | `pragma_base` | ✅ initial |
| Sync primitives (latches, parking, counters, scoped tasks) | `pragma_sync` | ✅ M1 |
| Work-stealing job system: `spawn`, `join`, `scope`, `parallel_for` | `pragma_jobs` | ✅ M1 |
| Per-frame task graph with declared resource access | `pragma_graph` | 📋 M2 |
| Async tasks for I/O and streaming | `pragma_async` | 📋 M3 |
| Facade with feature flags and `prelude` | `pragma_core` | ✅ initial |

## Principles

- **Pure Rust.** No C/C++ compiled into the dependency tree; enforced by `cargo deny` and
  `tools/audit_workspace.py`.
- **No panics in the runtime.** `unwrap`/`expect`/`panic!`/unchecked indexing are denied by lints;
  failures are typed errors. A panicking job never takes down a worker.
- **Contained `unsafe`.** Only `pragma_sync` may use it, verified with Miri and loom.
- **Deterministic by design.** Results never depend on which thread ran what.
- **Layered.** Each crate depends only on lower layers (`layers.toml`); subsystems sit behind features.

## Quick look

```rust
use pragma_core::prelude::*;

fn main() -> Result<(), pragma_core::base::BaseError> {
    let mut pool = HandlePool::new();
    let h = pool.insert(42_u32)?;
    assert_eq!(pool.get(h), Some(&42));
    pool.remove(h)?;
    assert_eq!(pool.get(h), None); // stale handles are safe
    Ok(())
}
```

## Platforms

Windows and Linux (x86_64) are first-class and tested in CI. Other targets are not
supported yet; see [DECISIONS.md](DECISIONS.md).

## Building

```bash
cargo test --workspace --all-features
```

The toolchain is pinned in `rust-toolchain.toml`; the minimum supported Rust version is 1.88.

## Documentation

- [Architecture](docs/architecture.md)
- [Reliability rules](docs/reliability.md)
- [Roadmap](docs/roadmap.md)
- [Decision log](DECISIONS.md)
- [Contributing](CONTRIBUTING.md)

## License

[MIT](LICENSE) © 2026 razed
