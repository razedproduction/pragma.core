# pragma.core

[![CI](https://github.com/razedproduction/pragma.core/actions/workflows/ci.yml/badge.svg)](https://github.com/razedproduction/pragma.core/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/pragma_core.svg)](https://crates.io/crates/pragma_core)
[![docs.rs](https://img.shields.io/docsrs/pragma_core)](https://docs.rs/pragma_core)
![MSRV](https://img.shields.io/badge/MSRV-1.88-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

**pragma.core** is a pure-Rust, solid and high-performance multithreaded core for
razed.production's game engines and frameworks.

It is the layer *under* an engine: it owns the threads, schedules the work of a frame
and runs background I/O, so that ECS, rendering, physics and audio can be written on top
of it without each inventing their own thread pool.

## Status

**1.0.0 — stable.** All milestones M0–M6 are done: the work-stealing job system, the
per-frame task graph, async tasks, observability, tuning, the hardening pass (determinism
suite, benchmark gate and semver checks in CI), the 1.0 API review, complete rustdoc with
an embedding checklist, and the [MSRV](#msrv-policy) and [versioning](#versioning) policy.
The public API follows semver: breaking changes happen only in a major release, and every
change is listed in the [changelog](CHANGELOG.md). See the [roadmap](docs/roadmap.md).

## Features

| Area | Crate | Facade |
|---|---|---|
| Typed errors, generational handles (`Handle`, `HandlePool`), simulation ticks | `pragma_base` | `base` |
| Sync primitives: latches, counters, task labels (plus the scheduler's internal building blocks) | `pragma_sync` | `sync` |
| Platform layer: thread names, priorities, core affinity (Windows/Linux) | `pragma_sys` | `sys` |
| Work-stealing pool: `join`, `scope`, `spawn`, `parallel_for`, `parallel_reduce`, priorities | `pragma_jobs` | `jobs` |
| Per-frame task graph with declared resource access, validated before the first frame | `pragma_graph` | `graph` |
| Async tasks on the pool, bounded blocking pool for synchronous I/O | `pragma_async` | `tasks` |
| Vertical slice: 100k entities, 9-system graph, background load, per-tick state hash | `examples/frame_sim` | — |

**Highlights:**

- **Help-while-wait.** Every wait (`join`, `scope`, `JobHandle::join`) runs other jobs, so
  nested parallelism never deadlocks, even on a single worker.
- **Panics are errors.** A panicking job, graph task or future returns `JobError::Panicked` /
  `AsyncError::Panicked`; the worker survives and nothing hangs.
- **Conflicts are caught at build time.** A graph whose tasks touch the same resource
  without an explicit order fails in `build()` with `GraphError::AmbiguousOrder`. It cannot
  become a data race in frame 10 000.
- **Allocation-free frames.** After the warm-up frame, a 50-task graph runs with zero heap
  allocations, with or without a `tracing` subscriber.
- **Deterministic.** Results do not depend on which thread ran what: `frame_sim` produces
  the same per-tick hash on 1, 2 and N workers, and CI checks this on every PR.
- **Observable.** `PoolStats` snapshots, `tracing` spans, optional [`puffin`](https://crates.io/crates/puffin)
  scopes, a debug-only watchdog for stuck jobs, and an allocation-free "which task is running
  here" query for panic hooks and crash reports.
- **Bounded memory.** Input-sized allocations go through `try_reserve` or a hard limit:
  you get `GraphError::TooLarge` / `BaseError::OutOfMemory` instead of an abort.

## Quick look

```rust
use pragma_core::prelude::*;

struct Velocity(f32);
struct Positions(Vec<f32>);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = ThreadPool::new(PoolConfig::default())?;

    // Fork-join and data-parallel loops; jobs may borrow from the stack.
    let (a, b) = pool.join(|| 1 + 1, || 2 + 2)?;
    assert_eq!((a, b), (2, 4));

    let mut data = vec![1_u32; 10_000];
    pool.parallel_for(&mut data, Grain::Auto, |x| *x *= 2)?;

    // Frame graph: declare what each task reads and writes; ordering is validated once.
    let mut g = FrameGraph::builder();
    g.task("input", |t| t.writes::<Velocity>(), |ctx| -> Result<(), GraphError> {
        ctx.write::<Velocity>()?.0 = 0.5;
        Ok(())
    });
    g.task("move", |t| t.reads::<Velocity>().writes::<Positions>().after("input"), |ctx| {
        let v = ctx.read::<Velocity>()?.0;
        let mut pos = ctx.write::<Positions>()?;
        ctx.pool().parallel_for(&mut pos.0, Grain::Auto, |p| *p += v)?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let mut graph = g.build()?;

    let mut world = Resources::new();
    world.insert(Velocity(0.0));
    world.insert(Positions(vec![0.0; 100_000]));
    graph.run(&pool, &mut world)?; // one frame

    // Background work: async on the pool, blocking I/O in a bounded side pool.
    let exec = Executor::new(&pool, AsyncConfig::default())?;
    let io = exec.clone();
    let mut load = exec.spawn_async(async move {
        let bytes = io.unblock(|| vec![0_u8; 64]).await?; // std::fs::read(...) in a real game
        Ok::<usize, AsyncError>(bytes.len())
    });
    let size = loop {
        if let Some(result) = load.poll_ready() {
            break result??; // check once per frame, never block the frame
        }
        std::thread::yield_now();
    };
    assert_eq!(size, 64);
    Ok(())
}
```

More examples are in the crate docs (`cargo doc --open -p pragma_core`) and in
[`examples/frame_sim`](examples/frame_sim).

## Using it

```toml
[dependencies]
pragma_core = "1.0"
```

Depend on the `pragma_core` facade rather than on the individual `pragma_*` crates (see
[Versioning](#versioning)).

| Feature | Default | Enables |
|---|---|---|
| `jobs` | ✅ | `ThreadPool` and the platform layer (`sys`) |
| `graph` | ✅ | `FrameGraph`, `Resources` (implies `jobs`) |
| `async` | ✅ | `Executor`, `AsyncHandle` (implies `jobs`) |
| `puffin` | — | Profiler scopes for pool operations and graph tasks; re-exports `puffin` |

`base` and `sync` are always available. Each feature builds and passes clippy on its own
(`cargo hack --each-feature` in CI).

## MSRV policy

The minimum supported Rust version is **1.88** (`rust-version` in `[workspace.package]`,
[DECISIONS #10](DECISIONS.md)), the minimum required by `proptest`.

- **Checked in CI.** The `MSRV (1.88)` job runs `cargo +1.88 check --workspace --all-features`
  on every PR and every push to `main`. Development itself uses the toolchain pinned in `rust-toolchain.toml`
  (1.99.0), so lints are identical locally and in CI.
- **Raised only in a minor release.** A higher MSRV is not treated as a breaking change, but
  it never ships in a patch release: it goes into the next minor version (`1.x.0`; `0.x.0`
  before 1.0), with its own line in the [changelog](CHANGELOG.md) and a
  [decision record](DECISIONS.md) explaining why ([DECISIONS #44](DECISIONS.md)).
- **One change, three places.** `rust-version`, the CI `msrv` job and the badge above move
  together in the same PR.

## Versioning

pragma.core follows [SemVer](https://semver.org/); the rules are in
[docs/reliability.md §8](docs/reliability.md#8-api-stability).

- **Lockstep.** All crates share one version (`version` in `[workspace.package]`, currently
  1.0.0): the `pragma_core` facade exposes all of them, so separate versions would give users
  nothing ([DECISIONS #33](DECISIONS.md)). A breaking change in any crate is a breaking
  release of all of them.
- **Exact internal dependencies.** The crates depend on each other with `=X.Y.Z`, so
  `pragma_jobs` 1.0.0 never builds against a `pragma_sync` 1.0.1 whose internals have moved
  ([DECISIONS #42](DECISIONS.md)). Depend on `pragma_core`, or keep all `pragma_*` crates at
  the same version.
- **Outside semver.** No compatibility guarantees cover:
  - `pragma_sync::__private` (hidden from the docs): the scheduler's building blocks, which
    change with every optimization of `join`/`scope`;
  - the `puffin` feature and the `pragma_core::puffin` re-export: `puffin` is not 1.0 yet,
    and moving to a new `puffin` version is a minor release.
- **Breaking changes.** Before 1.0, a minor version (`0.x.0`) may break the API, and every
  break is listed in the [changelog](CHANGELOG.md). After 1.0, breaks happen only in a major
  release; an item is first marked `#[deprecated(since, note)]` for at least one minor release.
- **Checked in CI.** `cargo semver-checks --workspace` runs on every PR against the PR's base
  on `main` (not against crates.io or a tag): a breaking change that comes without a version
  bump fails the PR that introduces it.

## Performance

Windows 10, AMD FX-8350 (8 logical cores), `cargo bench`. Ranges are min…max across A/B
rounds. Full method and numbers are in [docs/perf.md](docs/perf.md).

| Benchmark | pragma.core | rayon |
|---|---|---|
| single `join` | 284…288 ns | 1.47…1.51 µs |
| fib(20), ~11k nested `join` | 197…200 µs | 99…108 µs |
| `parallel_for`, 10⁶ elements | 454…478 µs | 477…508 µs |
| `parallel_reduce`, 10⁶ elements | 198…214 µs | 230…282 µs |
| frame graph, 50 near-empty tasks | 8.3…8.4 µs per frame | — |
| `frame_sim`, 100k entities | ~5 ms/tick on 1 worker, ~2 ms on 11 | — |

Nested fork-join is the one place where rayon is still ahead (about 2×). A CI gate fails any PR
that regresses a benchmark by more than 10% beyond runner noise.

## Reliability

Reliability here is a set of properties that CI checks on every PR. Details are in
[docs/reliability.md](docs/reliability.md).

- **Pure Rust.** No C/C++ compiled into the dependency tree; enforced by `cargo deny` and
  `tools/audit_workspace.py`. System FFI (`windows-sys`, `libc`) only.
- **No panics in the runtime.** `unwrap`/`expect`/`panic!`/unchecked indexing are denied by
  lints; failures are `#[non_exhaustive]` typed errors with context.
- **Contained `unsafe`.** Only in `pragma_sync` (lifetime erasure, on-stack jobs, task labels)
  and `pragma_sys` (OS calls). Every occurrence has a budget in `layers.toml`; any growth
  needs a new decision record. All of it is reviewed in [docs/unsafe.md](docs/unsafe.md).
- **Verified concurrency.** Every atomic protocol has a loom model. `pragma_sync` runs under
  Miri, and a one-worker pool runs under Miri with Tree Borrows.
- **Tested in depth.** Unit, property (`proptest`) and stress tests, plus fuzzing of the graph
  builder against an independent oracle. Nightly CI runs 30-minute fuzzing and soak tests.
- **Layered.** Each crate depends only on lower layers (`layers.toml`):

```
L4  pragma_core     facade: re-exports, prelude, features
L3  pragma_graph    frame graph           pragma_async   async tasks
L2  pragma_jobs     work-stealing pool
L1  pragma_sync     sync primitives       pragma_sys     platform layer
L0  pragma_base     errors, handles, ticks
```

## Platforms

Windows and Linux (x86_64) are first-class and tested in CI. Other targets are not
supported yet; the API does not rule out a single-threaded mode for wasm later. See
[DECISIONS.md](DECISIONS.md).

## Building

```bash
cargo test --workspace --all-features
cargo run -p frame_sim --release -- --ticks 300 --verify 1,4
```

The toolchain is pinned in `rust-toolchain.toml` (1.99.0); the minimum supported Rust version
is 1.88 ([MSRV policy](#msrv-policy)). See [CONTRIBUTING.md](CONTRIBUTING.md) for the full check list (Miri, loom, fuzz, benches).

## Documentation

- API reference: [docs.rs/pragma_core](https://docs.rs/pragma_core) (or `cargo doc --open -p pragma_core`). Every
  public type and free function has an example; the crate page of `pragma_core` has the section
  **"Integrating into an engine"**: pool ownership, the graph in the game loop, async results,
  priorities, panics, shutdown order and determinism
- [Architecture](docs/architecture.md): layers and the execution model
- [Reliability rules](docs/reliability.md)
- [Performance methodology and results](docs/perf.md)
- [`unsafe` review](docs/unsafe.md)
- [Roadmap](docs/roadmap.md)
- [Decision log](DECISIONS.md)
- [Changelog](CHANGELOG.md)
- [Contributing](CONTRIBUTING.md)

## License

[MIT](LICENSE) © 2026 razed
