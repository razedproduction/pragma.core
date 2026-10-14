# Changelog

The format is [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning is [SemVer](https://semver.org/).

## [Unreleased]

## [1.0.0] — 2026-10-14

The first stable release (DECISIONS #47). The public API is the one reviewed in 0.6.0; from now on
breaking changes happen only in a major release. All crates were already published once on
crates.io as 0.6.0 (from `main` after M6c, before this release's metadata); 1.0.0 is the
recommended version.

### Changed

- Version 1.0.0 for all crates; internal dependencies are `=1.0.0`.

- All project documentation is now in English: rustdoc and code comments, test messages,
  `docs/`, `DECISIONS.md`, `CONTRIBUTING.md`, this changelog, CI comments, GitHub templates and the
  output of `tools/*.py` (DECISIONS #43, supersedes #11). No API changes.
- CI: a PR that changes only Markdown files and `docs/` skips fuzz, Miri, semver-checks and the
  bench gate (DECISIONS #45). Pushes to `main` still run everything.

### Added

- crates.io metadata: every crate has `keywords`, `categories`, its own `LICENSE` and a short
  README (`pragma_core` uses the project README); `pragma_core`, `pragma_jobs` and `pragma_graph`
  are documented on docs.rs with all features (`[package.metadata.docs.rs] all-features = true`).
- README sections "MSRV policy" and "Versioning": MSRV 1.88, raised only in a minor release with
  a changelog line (DECISIONS #44); lockstep versions, exact internal dependencies, what is
  outside semver, how breaking changes are handled before and after 1.0. No API changes.
- Rustdoc examples (doctests) for every public type and free function and the main methods of all
  crates, and the section "Integrating into an engine" in the `pragma_core` crate docs: pool
  ownership and worker count, the frame graph in the game loop, `poll_ready` instead of `wait`,
  background priorities, the panic hook, `panic = "abort"`, shutdown order and determinism, with a
  compiled engine skeleton (DECISIONS #46). No API changes.

## [0.6.0] — 2026-10-14

### Changed (breaking) — M6a "Public API review" (DECISIONS #42)

- `pragma_sync`: only `CountLatch`, `AtomicCounter`, `SyncError`, `TaskLabel`,
  `current_label` are public. `ArrivalCounter`, `Parker`/`Unparker`, `scope`/`TaskScope`/`ScopedTask`/
  `StackSlot`, `join_on_stack`/`StackJob`/`JoinLatch`/`NO_OWNER`, `run_labeled`,
  `Backoff`/`CachePadded` moved to the hidden `pragma_sync::__private` (outside semver).
- `pragma_core::sync` is a module with the stable part of `pragma_sync` instead of a crate re-export.
- Removed the `pragma_base::tracing` re-export: install a subscriber through your own `tracing` 0.1 dependency.
- Error variants with fields (`BaseError`, `SysError`, `JobsError`, `JobError`, `GraphError`,
  `AsyncError`) are `#[non_exhaustive]`: patterns need `..`, and they cannot be constructed outside the crate.
- `pragma_graph::TaskResult` is sealed (implemented for `()` and `Result<(), E>`).
- Internal crates depend on each other with an exact version (`=0.6.0`).

### Added

- `impl Debug for HandlePool<T>` (`T: Debug`); the `missing_debug_implementations` lint for the whole workspace.
- The `puffin` feature and the `pragma_core::puffin` re-export are explicitly declared outside semver guarantees.

### Removed

- The unused workspace dependency `crossbeam-channel`.

### Changed

- Nested `join` is ~10% faster: after pushing the second branch the worker checks for sleepers without
  `fence(SeqCst)`; a lost wakeup here costs only parallelism (DECISIONS #40).
  fib(20) is ~1.8× rayon (was ~2×).
- Bench gate: the `frame_sim` threshold is computed from the spread of its own runs rather than from the noise
  of the nanosecond `rayon_single` (DECISIONS #41).

### Fixed

- `ThreadPool::new` returns only after every worker has finished thread initialization
  (registration in `crossbeam-epoch`, the worker context, priority and affinity). Previously
  a worker that got no core under oversubscription (4 workers on 2 vCPUs) made these
  one-time allocations in the middle of the first frames, and `tests/alloc.rs` occasionally failed in CI on
  windows-latest (DECISIONS #39).

## [0.5.0] — 2026-10-14

The first numbered version: before it all crates were 0.0.1, although milestones M1–M4 were called 0.1–0.4
(DECISIONS #33). Below is everything that went into M1–M5.

### Added — M5b "Memory and `unsafe`"
- `GraphError::TooLarge { tasks, reason }`, `pragma_graph::MAX_TASKS` (16,384) and `MAX_EDGES` (2²²):
  `build()` no longer allocates n²/8 bytes on any input; the reachability matrix and edges go
  through `try_reserve` (DECISIONS #37).
- `BaseError::OutOfMemory { slots }`: `HandlePool::insert` grows via `try_reserve`, and `remove`
  does not allocate.
- The `pragma_base/tests/oom.rs` test with a failing global allocator; a proptest and a fuzz case
  (the `0xFE` prefix) for large graphs.
- `pragma_jobs/tests/miri.rs`: a 1-worker pool under Miri (Tree Borrows) in CI (DECISIONS #38).
- `docs/unsafe.md`: a review of all 52 `unsafe` occurrences.

### Changed — M5b
- The version of all crates is 0.5.0.
- `pragma_sys::set_current_thread_name` on Windows truncates the name to 256 UTF-16 units.
- Reliability rule §1: an allocation whose size the input sets goes only through `try_reserve`
  or under a limit.

### Added — M5a "Guarantees in CI"
- The mandatory determinism suite: `frame_sim --suite` (5 scenarios × 1/2/N workers ×
  instant/delayed load + `idle_spin` 0/128, `PoolStats` snapshots, watchdog, a `tracing`
  subscriber) in CI on Windows and Linux; the debug variant is `frame_sim/tests/suite.rs` (DECISIONS #35).
  `SimConfig` got `idle_spin`, `watchdog`, `sample_stats`; the output got an `ms/tick` column.
- The `tools/bench_gate.py` bench gate: an A/B of `main` ↔ PR on one runner; a regression > 10% beyond
  the reference noise fails the PR; the fib(20) ≤ 2× rayon headroom is in the summary (DECISIONS #34).
- `cargo-semver-checks` in CI with `main` as the baseline (DECISIONS #33).
- The `unsafe` counter: `[unsafe_budget]` in `layers.toml`, checked in `tools/audit_workspace.py`;
  in a PR, budget growth without a new DECISIONS entry fails CI (DECISIONS #36).
- Nightly CI: 30 minutes of graph builder fuzzing with a persisted corpus; soak: the
  `pragma_jobs`/`pragma_async` stress tests 5 times longer with a new seed (`PRAGMA_SOAK_ROUNDS`,
  `PRAGMA_SOAK_SEED`); the determinism suite over 3000 ticks.

### Changed — M4b "Tuning"
- `ThreadPool::join` on a worker and in an external slot: the second branch lives on the stack with a latch
  (`pragma_sync::join_on_stack`, DECISIONS #30): a branch that was not stolen is run by the owner
  without atomic RMW. fib(20): 538 → ~200 µs (≤ 2× rayon, was 4.6–5.5×).
- Before stealing, a worker checks `is_empty` (without entering a crossbeam epoch) and registers in
  `crossbeam-epoch` at startup rather than at its first steal in the middle of a frame.
- Every external slot has its own `Parker`: the owner of a `join` waiting for a stolen branch sleeps until
  the thief wakes it instead of polling a counter.

### Added — M4b
- `pragma_sync::join_on_stack`, `StackJob`, `JoinLatch`, `NO_OWNER` (+ a loom model and Miri).
- `PoolConfig::with_idle_spin` / `DEFAULT_IDLE_SPIN` (32): how many rounds a worker searches for work
  before parking (DECISIONS #32).
- `docs/perf.md`: hardware, commands, number of runs and variance, before/after results.
- The `pragma_async/benches/async.rs` bench: the cost of rescheduling an async task and of the `Box` in it
  (the `Box` is kept, ~6% of the cost, DECISIONS #31).

### Added — M4a "Observability"
- A new crate `pragma_sys` (L1, `unsafe` for system FFI only, DECISIONS #27):
  `set_current_thread_name`, `set_current_thread_priority` / `current_thread_priority`
  (`ThreadPriority::{Low, Normal, High}`), `pin_current_thread`, `current_core`; errors are
  `SysError`. Available as `pragma_jobs::sys` and `pragma_core::sys`.
- `PoolConfig::with_thread_priority`, `with_affinity` (worker `i` → `cores[i % len]`): an OS
  refusal is a warning in the log, and the worker runs without the setting.
- The current task context for crash reports (DECISIONS #28): `pragma_sync::run_labeled` /
  `current_label` / `TaskLabel`; in `pragma_jobs`, `labeled(kind, name, f)` and `current_task(f)`.
  Labels on graph tasks, `spawn`, `scope`/`parallel_*` tasks, `spawn_detached`, async and `unblock`;
  reading is allocation-free and fit for a panic hook.
- `ThreadPool::spawn_named`, `Executor::spawn_async_named`: the task name in the label and in the panic error.
- `ThreadPool::stats() -> PoolStats` (`ThreadStats` per worker and for external threads): executed,
  stolen, parks, idle and sleep time, queue depth. No locks and no RMW on the hot path.
- A stuck-task watchdog (debug only): `PoolConfig::with_watchdog(Option<Duration>)`, defaulting to
  `DEFAULT_WATCHDOG` = 5 s; one `tracing::warn!` (target `pragma_jobs::watchdog`) per
  task. No code in release.
- `tracing`: the `graph_task` (task, priority, worker) and `frame_graph` spans at `DEBUG`;
  `scope`/`parallel_for`/`parallel_reduce` at `TRACE`.
- The `puffin` feature (`pragma_jobs`, `pragma_graph`, the facade; puffin 0.19.1, DECISIONS #29):
  profiler scopes for the frame, graph tasks and pool operations; the facade re-exports `puffin`.
- `ThreadPool::current_worker_index`, `pragma_jobs::current_priority`, `Priority::as_str`.
- Tests: `pragma_jobs/tests/observability.rs`, `tests/watchdog.rs` (debug and release),
  `pragma_core/tests/crash_context.rs` (the panic hook names a graph task / spawn / async /
  unblock); `pragma_graph/tests/alloc.rs`: an allocation-free frame also with a `tracing` subscriber, and reading
  the label from a panic hook without allocations.

### Changed — M4a
- The `JobError` of a cancelled/failed `spawn_named` task carries its name instead of `spawn`.

### Added — M3 "Async tasks and the vertical slice"
- `pragma_async`: `Executor::new(&pool, AsyncConfig)`, an executor on `async-task` on top of the pool.
  `spawn_async(fut) -> AsyncHandle<T>`: the future runs on the workers with `Background`
  priority; a panic is `AsyncError::Panicked { task, message }`.
- `Executor::unblock(f)`: synchronous I/O in a bounded blocking pool (threads on demand,
  `AsyncConfig::with_blocking_threads` / `with_blocking_capacity`); overflow is immediately
  `AsyncError::Overloaded`, and the caller does not wait (DECISIONS #23).
- `AsyncHandle`: cancellation by drop, `detach()`, the frame bridge `poll_ready()`, `Future` for `.await`,
  `wait()` outside pool work (from a worker or a graph frame it is `AsyncError::WouldBlock`, DECISIONS #25).
- `AsyncError`: the `Overloaded`, `SpawnFailed`, `WouldBlock`, `Consumed`, `InvalidConfig` variants.
- `pragma_jobs::Spawner` (`ThreadPool::spawner()`): a `Weak` reference to the pool, `spawn_detached`,
  `is_alive`, `in_pool_context` (DECISIONS #24).
- The `examples/frame_sim` example (a workspace member, layer 5): 100k entities, 9 graph systems,
  background map loading via `spawn_async` + `unblock`, a state hash on every tick;
  `--verify 1,4` compares hashes across worker counts and load timings (DECISIONS #26).
- Tests: `pragma_async/tests/executor.rs` (cancellation at every stage with a `Drop` counter, panics,
  the blocking pool, pool/executor shutdown), a proptest and an `#[ignore]` stress test (`tests/prop.rs`);
  `frame_sim/tests/determinism.rs`.
- CI: a `frame_sim --verify 1,4` step on Windows and Linux; a nightly `pragma_async` stress run.
- Facade: `AsyncConfig`, `AsyncError`, `AsyncHandle`, `Executor` in the `prelude` (feature `async`).

### Changed — M3
- `pragma_async::AsyncError::Panicked` got a `task` field (context: `spawn_async` / `unblock`).
- `pragma_jobs`: the destructor of a task dropped without running (pool shutdown) can no longer
  bring down the stopping thread with a panic; it is caught and logged.

### Added — M2 "Frame graph"
- `pragma_graph`: `FrameGraph::builder()`, tasks with `reads::<T>()` / `writes::<T>()` and explicit
  `before`/`after`; the `ordered` mode (a conflict is ordered by declaration order).
- `build()` before the first frame: `GraphError::DuplicateTask`, `UnknownDependency`, `Cycle` (with
  the names of the cycle's tasks), `AmbiguousOrder` (an unordered access conflict, with the resource).
- A stable topological sort (ties broken by registration order) and transitive reduction of edges;
  `FrameGraph::order()`, `edges()`.
- `FrameGraph::run(&pool, &mut resources)`: dataflow execution without allocations in the steady
  state; independent tasks in parallel. A task error/panic (`TaskFailed`,
  `TaskPanicked`) skips its successors without stopping independent branches; the error of the task
  with the lowest index is returned.
- `Resources`: typed slots (an `RwLock` per type) with `insert`/`remove`/`get`/`get_mut`;
  `TaskContext::read`/`write` check the declared access (`UndeclaredAccess`,
  `BorrowConflict`, `MissingResource`). The graph does not own the resources and can run over
  several instances.
- Nested parallelism: `TaskContext::pool()` for `parallel_for`/`join`/`scope` from a task.
- Graph export: `to_dot()` (Graphviz) and `to_mermaid()`.
- `TaskResult`: a system returns `()` or `Result<(), E>`; a `GraphError` from `?` propagates as is.
- `pragma_sync::ArrivalCounter`: "the last of N arrivals continues" (+ a loom model).
- Tests: behavioral (`tests/graph.rs`), "an allocation-free frame" with a counting allocator
  (`tests/alloc.rs`), a proptest against an independent oracle (`tests/prop.rs`, `tests/oracle`),
  builder fuzzing (`fuzz/`, cargo-fuzz). The frame bench: `cargo bench -p pragma_graph`.
- CI: `cargo hack clippy --each-feature`, 60 s of graph fuzzing.
- Facade: `FrameGraph`, `GraphError`, `Resources`, `TaskContext` in the `prelude` (feature `graph`).

### Changed — M2
- `pragma_jobs`: an external thread reuses the cached context of a pool slot, so entering the pool
  (for example once per frame) no longer allocates (DECISIONS #18).
- `pragma_sync`: the scope counter cache takes back a counter at zero even if an `Arc` clone is still
  alive in a finishing task, so cache misses no longer allocate.
- `GraphError` got the `DuplicateTask`, `MissingResource`, `UndeclaredAccess`,
  `BorrowConflict`, `TaskFailed`, `TaskPanicked`, `Job` variants (the enum is `#[non_exhaustive]`).

### Added — M1 "Job system"
- `pragma_jobs`: `ThreadPool::new(PoolConfig) -> Result` with named threads (`<prefix>-<index>`)
  and a `Drop` that stops and `join`s all threads (`ShutdownPolicy::{Drain, Cancel}`).
- Work-stealing: local LIFO `crossbeam_deque::Worker` queues per priority, global
  `Injector`s, stealing from a random victim; targeted wakeup of one sleeping worker.
- `spawn(Priority, f) -> JobHandle<T>`, `JobHandle::join() -> Result<T, JobError>`.
- `join(a, b)`, `scope(|s| ...)` with borrowed data, `parallel_for`, `parallel_for_chunks`,
  `parallel_reduce` (a deterministic fold in chunk order), `Grain::{Auto, Fixed(n)}`.
- Help-while-wait in every wait: nested `join`/`scope` do not deadlock even on a single worker;
  an external thread (the main one) takes part in the work through the pool's external slots.
- Panic catching: `JobError::Panicked { job, message }`, `JobError::Cancelled { job }`.
- The `Frame` / `Normal` / `Background` priorities, a worker limit for `Background`
  (`PoolConfig::with_background_limit`).
- `pragma_sync`: `Parker`/`Unparker`, `AtomicCounter` (+ `wait_while_helping`), `scope()` /
  `TaskScope` / `ScopedTask` (lifetime erasure of scope tasks), `StackSlot` (a task without
  allocation), `SyncError`.
- loom models of the latch, parking and the counter (`crates/pragma_sync/tests/loom.rs`).
- Tests: behavioral (`tests/pool.rs`), proptests (`parallel_for`/`parallel_reduce` equal
  sequential code, nested `scope`s of random depth), an `#[ignore]` stress test
  (10⁷ tasks, random panics, 1..2N workers) and a nightly workflow for them.
- `criterion` benches against `rayon`: `cargo bench -p pragma_jobs`.

### Changed
- `pragma_sync::CountLatch::new` is no longer a `const fn` (needed for the loom models).
- `loom` is attached to `pragma_sync` as `[target.'cfg(loom)'.dependencies]`;
  the CI loom job runs `--lib --tests` (DECISIONS #12).
- `deny.toml`: the graph is restricted to the Windows/Linux x86_64 target platforms.

### Added — M0
- A layered workspace: `pragma_base`, `pragma_sync`, `pragma_jobs`, `pragma_graph`, `pragma_async`, the `pragma_core` facade.
- `pragma_base`: `Handle`/`HandlePool` (generational), `Tick`, `BaseError`.
- `pragma_sync`: `CountLatch`; re-exports of `Backoff`, `CachePadded`.
- `pragma_jobs`: `PoolConfig` with validation, `JobsError`.
- CI (Windows + Linux), `cargo-deny`, the purity and layer audit, Miri and loom.
- Documentation: architecture, reliability rules, roadmap, DECISIONS.
