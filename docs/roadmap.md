# pragma.core roadmap

The principle: **a vertical slice before breadth**. Already by M3 the `frame_sim` example goes through every layer:
pool → frame graph → async loading → a deterministic result. Every following milestone is verified
on it rather than on isolated microtests.

Legend: ✅ done · 🚧 in progress · 📋 planned

| Milestone | Version | Essence | Status |
|---|---|---|---|
| M0 | 0.0.1 | Repository foundation | ✅ |
| M1 | 0.1.0 | Job system | ✅ (the nested `join` debt was closed in M4b) |
| M2 | 0.2.0 | Frame graph | ✅ |
| M3 | 0.3.0 | Async tasks and the `frame_sim` vertical slice | ✅ |
| M4 | 0.4.0 | Observability and tuning | ✅ |
| M5 | 0.5.0 | Hardening: determinism, fuzz, bench gate, semver | ✅ (M5a is PR #8, M5b is PR #9; landed in `main` as a single squash, #8) |
| M6 | 1.0.0 | Stable API, publication | 🚧 (M6a: API review, 0.6.0; M6b: documentation in English; M6d: MSRV and versioning policy) |

---

## M0 — Foundation ✅

- [x] Layered workspace (`layers.toml`), edition 2024, pinned toolchain, MSRV 1.88
- [x] Strict lints (no panics), `forbid(unsafe_code)` everywhere except `pragma_sync`
- [x] `deny.toml` (licenses, ban on C crates), `tools/audit_workspace.py`
- [x] CI: Windows + Linux, fmt, clippy, test, doc, MSRV, deny, audit, Miri, loom
- [x] `pragma_base`: `Handle`/`HandlePool` (+ a property test), `Tick`, errors
- [x] `pragma_sync`: `CountLatch`, re-exports of `Backoff`/`CachePadded`
- [x] `pragma_jobs`: `PoolConfig` + validation; skeletons of `pragma_graph`, `pragma_async`, the `pragma_core` facade
- [x] Docs: architecture, reliability, roadmap, DECISIONS, CONTRIBUTING

## M1 — Job system (0.1.0)

**Goal:** a pool on which real frame work can be parallelized without deadlocks or lost panics.

`pragma_sync`
- [x] `Parker`/`Unparker` for sleeping workers (targeted wakeup, no lost wakeups)
- [x] `AtomicCounter` that tasks are waited on ("wait for the counter to reach zero" instead of a handle per task)
- [x] loom models: latch, parking, counter

`pragma_jobs`
- [x] `ThreadPool::new(PoolConfig) -> Result`, named threads, a correct `Drop` (joins all threads)
- [x] Work-stealing: local `crossbeam_deque::Worker` + `Injector` + stealing from a random victim
- [x] `spawn` → `JobHandle<T>` with `join() -> Result<T, JobError>`
- [x] `join(a, b)` with help-while-wait; a nested `join` on 1 worker does not deadlock
- [x] `scope(|s| ...)` with borrowed data (no `'static`)
- [x] `parallel_for` / `parallel_for_chunks` with `Grain::{Auto, Fixed(n)}`; a deterministic reduction (`parallel_reduce`)
- [x] Panic catching → `JobError::Panicked { job, message }`
- [x] `Frame` / `Normal` / `Background` priorities + a worker limit for `Background`

Tests and benches
- [x] proptest: `parallel_for` equals a sequential loop; nested `scope`s at random depth
- [x] Stress (`#[ignore]`, nightly in CI): 10⁷ tiny tasks, random panics, 1..2N workers
- [x] `criterion`: spawn/join overhead, `parallel_for` over 10⁶ elements, comparison with `rayon`

**Done when:** everything above is green on Windows and Linux; the spawn+join overhead is ≤ 2× of
`rayon::join`; a panic in a task does not crash the process and nothing hangs; the pool's `Drop` leaves no threads.

**Criteria status** (Windows, AMD FX-8350, 8 logical cores — previously misstated as "12 threads", see `docs/perf.md`, `cargo bench -p pragma_jobs`):

| Criterion | Status |
|---|---|
| Green on Windows and Linux | Locally Windows; Linux in PR CI |
| A panic does not crash the process, nothing hangs | ✅ `pool.rs` tests + the `random_panics_never_hang_or_kill_workers` stress test |
| `Drop` leaves no threads | ✅ `Drop` joins all threads; a stress test of 500 create/drop cycles |
| A single `join` ≤ 2× rayon | ✅ 0.65 µs vs 1.86 µs |
| Nested fork-join (fib(20), ~11k `join`s) ≤ 2× rayon | ✅ in M4b: 197–200 µs vs 99–108 µs (1.95–1.99×; was 0.56 ms vs 0.12 ms, 4.6×), DECISIONS #30, `docs/perf.md` |
| `parallel_for` / reduction over 10⁶ | ✅ 0.51 / 0.30 ms vs 0.61 / 0.34 ms for rayon |

## M2 — Frame graph (0.2.0)

- [x] `FrameGraph::builder()`: tasks declaring `reads::<T>()` / `writes::<T>()` and explicit `before`/`after`
      (+ the `ordered` mode: a conflict is ordered by declaration order)
- [x] `build()`: cycles → `GraphError::Cycle`, an implicit conflict → `AmbiguousOrder`, an unknown dependency
      → `UnknownDependency`, a duplicate name → `DuplicateTask`
- [x] A stable topological sort; `run()` without allocations in the steady state (the `tests/alloc.rs` test)
- [x] Resource storage with safe split access (typed slots, borrow checking when building the graph)
- [x] Nested parallelism: a graph task can call `parallel_for`
- [x] Graph export to DOT/Mermaid for debugging
- [x] Fuzz: arbitrary graph descriptions → either an error or a correct order (`fuzz/`, the same oracle in proptest)
- [x] CI: `cargo hack --each-feature`

**Done when:** a graph of ~50 tasks is built once and runs a frame without allocations;
any unordered conflict is caught in `build()`.

**Criteria status** (Windows, AMD FX-8350, 8 logical cores — previously misstated as "12 threads", see `docs/perf.md`):

| Criterion | Status |
|---|---|
| ~50 tasks, an allocation-free frame | ✅ `tests/alloc.rs`: 50 tasks, 0 allocations over 2000 frames on 1/2/4 workers (debug and release) |
| Any unordered conflict is caught in `build()` | ✅ `AmbiguousOrder` before the first frame; proptest + fuzz against an independent oracle; resource access never conflicts during a frame |
| Green on Windows and Linux | Locally Windows; Linux, fuzz and `cargo hack` in PR CI |
| Performance (`cargo bench -p pragma_graph`, 50 tasks, two independent chains) | 4096 elements of work per task: 313 µs vs 714 µs sequentially; near-empty tasks: 9.2 µs vs 2.8 µs, ~130 ns of scheduling per task |

## M3 — Async tasks and the vertical slice (0.3.0)

- [x] An executor on `async-task`: `spawn_async(fut) -> AsyncHandle<T>`, scheduling into the pool with `Background` priority
      (via `pragma_jobs::Spawner`, DECISIONS #24)
- [x] A blocking pool for synchronous file I/O (bounded in size): `Executor::unblock`; the I/O model is DECISIONS #23
- [x] Cancellation by dropping the handle, a panic → `AsyncError::Panicked`
- [x] The bridge into the frame: `poll_ready()` (+ `.await` on the handle), no `block_on` in frame tasks: `wait()` from pool
      work returns `WouldBlock` (DECISIONS #25)
- [x] **The `examples/frame_sim` example**: a headless fixed-step loop, 100k "entities",
      a graph of 9 systems, background "loading" via async, a state hash on every tick
- [x] `frame_sim` runs in CI for N ticks and compares the hash on 1 and N workers

**Done when:** `frame_sim` runs stably in CI on both OSes and the hash matches.

**Criteria status** (Windows, AMD FX-8350, 8 logical cores — previously misstated as "12 threads", see `docs/perf.md`):

| Criterion | Status |
|---|---|
| `frame_sim` in CI on both OSes, the hash matches on 1 and N workers | Locally Windows: `--ticks 300 --verify 1,4,11`, the hash of all 300 ticks matches across 6 runs; Windows and Linux: the `--verify 1,4` CI step in PRs |
| The result does not depend on when the load finishes | ✅ the load is applied at tick 60 (DECISIONS #26); `--verify` runs both "in time" (ready at tick 1) and "late" (250 ms, the main thread waits between frames), and the hashes are equal; the `background_load_affects_state_from_apply_tick` test confirms the load affects the state |
| Cancellation without leaks or hangs | ✅ `tests/executor.rs`: dropping before the start, during `poll`, while waiting, after completion; a `Drop` counter; stopping the pool and the executor gives `Cancelled` without hanging |
| A panic → `AsyncError::Panicked`, the worker is alive | ✅ on 1 worker, for `spawn_async` and `unblock` |
| Blocking pool: a limit, overflow does not block | ✅ the peak ≤ the thread limit; beyond `blocking_capacity`, `Overloaded` immediately; pool workers are free while a blocking thread is busy |
| Stress | ✅ a proptest (random cancellation/panics/blocking, 1–3 workers) + an `#[ignore]` run of 200k tasks on 1..2N workers (0.5 s), nightly CI |
| `frame_sim` performance (100k entities, release) | 1 worker ~5 ms/tick, 4 ~2.4 ms, 11 ~2 ms |

## M4 — Observability and tuning (0.4.0)

M4a (observability):
- [x] `tracing` spans on graph tasks and `scope` (+ `parallel_*`, the graph frame)
- [x] Pool metrics: executed, steals, idle time, parks, queue depth (`PoolStats`)
- [x] The `puffin` feature: frame profiling (puffin 0.19.1, DECISIONS #29)
- [x] The `pragma_jobs::sys` platform layer (the `pragma_sys` crate, DECISIONS #27): thread names and priorities,
      optional affinity (Windows/Linux)
- [x] A stuck-task watchdog in debug builds
- [x] The current task context for crash reports: a cheap (allocation-free) query "which task/graph node
      is running on this thread right now", available from a panic hook and the engine's crash handler, so the
      culprit's name is not lost even on `abort` (DECISIONS #28)

M4b (tuning):
- [x] spin→park tuning by benchmarks (`PoolConfig::with_idle_spin`, DECISIONS #32); the measurement methodology document is `docs/perf.md`
- [x] Nested `join` overhead ≤ 2× rayon (the M1 debt: was 4.6× on fib(20), now 1.95–1.99×, DECISIONS #30):
      a latch without `Arc` and fewer atomic RMWs per `join`, a cached external slot context
- [x] No `Box` per async scheduling, or a bench showing it does not matter: benched, the `Box` is ~6% of the rescheduling cost, kept (DECISIONS #31)

**M4a criteria status** (Windows, AMD FX-8350, 8 logical cores — previously misstated as "12 threads", see `docs/perf.md`; before → after, `cargo bench`, `frame_sim --ticks 300`):

| Criterion | Status |
|---|---|
| An allocation-free graph frame with and without a `tracing` subscriber | ✅ `tests/alloc.rs`: 0 allocations over 2000 frames on 1/2/4 workers in both modes (debug and release) |
| The panic hook names the culprit, without allocations | ✅ `pragma_core/tests/crash_context.rs` (graph, `spawn`, async, `unblock`); reading the label in the hook makes 0 allocations (`tests/alloc.rs`) |
| Watchdog: one warning with the name in debug, nothing in release | ✅ `pragma_jobs/tests/watchdog.rs`; the release run is a CI step |
| `PoolStats`: the sums add up, a snapshot does not block | ✅ `tests/observability.rs`: 180 tasks = 180 in the snapshot; a snapshot with busy workers sees the queue |
| Determinism | ✅ `frame_sim --verify 1,4,11`: the hash of all 300 ticks matched across 3 runs |
| No regressions (rayon noise between runs is up to ±8%) | single `join` 0.53 → 0.55 µs; fib(20) 581 → 538 µs; `scope` 1000 965 → 911 µs; `spawn`+`join` 1.32 → 1.37 µs; `parallel_for` 460 → 454 µs; reduction 214 → 200 µs; a 50-task graph 9.16 → 9.32 µs / 292 → 279 µs; `frame_sim` ms/tick: 1 worker 4.97 → 5.06, 4: 2.39 → 2.32, 11: 1.88 → 1.78 |

**M4b criteria status** (the same machine; A/B of the branch ↔ `main`, 2 rounds, methodology in `docs/perf.md`):

| Criterion | Status |
|---|---|
| Nested `join` (fib(20)) ≤ 2× rayon | ✅ 196.6–199.7 µs vs 99.0–108.0 µs for rayon (1.95–1.99×); `main`: 490.7–504.0 µs |
| spin→park tuning, methodology | ✅ a sweep of `idle_spin` 0/8/32/128, default 32; `docs/perf.md` (hardware, commands, A/B, variance) |
| `Box` per async scheduling | ✅ shown by a bench not to matter: 88 ns out of 1.39 µs (~6%), kept (DECISIONS #31) |
| No regressions beyond noise | ✅ `scope`, `parallel_*`, graph, `frame_sim` within the spread; single `join` 0.53–0.57 → 0.28 µs; `spawn`+`join` +5–10%, within the bench's spread (1.30–1.61 µs on the same code) |
| Determinism | ✅ `frame_sim --verify 1,4,11`: the hash matched in all runs |
| An allocation-free graph frame | ✅ `tests/alloc.rs` (caught a one-time registration in `crossbeam-epoch` in the middle of a frame; fixed, DECISIONS #30) |
| loom / Miri for the new protocol | ✅ the `stack_join_owner_never_misses_thief` model, `join_on_stack` tests under Miri |

## M5 — Hardening: "pro" guarantees (0.5.0)

M5a (guarantees in CI):
- [x] A mandatory determinism test in CI (1/2/N workers, several scenarios): `frame_sim --suite`
      + `tests/suite.rs`; observability does not change the hash (DECISIONS #35)
- [x] Bench gate: a regression > 10% fails CI (compared with `main`): `tools/bench_gate.py`, an A/B on one
      runner (DECISIONS #34)
- [x] `cargo-semver-checks` in CI, the baseline is `main` (DECISIONS #33)
- [x] Nightly fuzz (30 min, the corpus is persisted) and scheduled soak tests
- [x] An unsafe-block counter in CI: `[unsafe_budget]` in `layers.toml` (DECISIONS #36)

M5b (memory and `unsafe`):
- [x] Miri for `pragma_jobs` tests in a single-threaded configuration: `tests/miri.rs`, Tree Borrows (DECISIONS #38)
- [x] A review of all `unsafe` in `pragma_sync`, `pragma_sys` and the test allocators: [`docs/unsafe.md`](unsafe.md)
- [x] Graph `build()` memory is bounded: `closure` in `plan.rs` took n²/8 bytes (100k tasks ≈ 1.25 GB);
      `MAX_TASKS` = 16,384, `MAX_EDGES` = 2²², `try_reserve` → `GraphError::TooLarge`; a fuzz case and a proptest for large n
- [x] `HandlePool::insert`: `slots` (and the free list) grow via `try_reserve` → `BaseError::OutOfMemory` instead of `abort`
- [x] A rule in `reliability.md`: an allocation whose size the input sets goes only through `try_reserve` or under a limit (DECISIONS #37)
- [x] The version of all crates is 0.5.0 (DECISIONS #33)

**Done when:** the determinism suite, the bench gate, semver-checks and the `unsafe` counter are green in CI on
both OSes, nightly fuzz/soak run on schedule; large inputs give errors instead of `abort`; Miri for
`pragma_jobs` is green; the `unsafe` review is written up.

**Criteria status** (Windows, AMD FX-8350, 8 logical cores; CI on GitHub Windows and Linux runners):

| Criterion | Status |
|---|---|
| The determinism suite in CI on both OSes | ✅ `frame_sim --suite --ticks 200`: 5 scenarios, 60 runs, the hash matched (20 s locally); the debug variant `tests/suite.rs` with a 1 ms watchdog and a `tracing` subscriber; CI of PR #8 |
| Observability does not change the hash (the M4 promise) | ✅ `idle_spin` 0/128, `PoolStats` every frame, watchdog off/1 ms, a `tracing` subscriber are all in the suite |
| Bench gate | ✅ an A/B on one runner, 3 rounds; locally (1 round) everything within limits, reference noise 9.6%; GitHub runner noise is in `docs/perf.md` §6 |
| fib(20) ≤ 2× rayon headroom | ⚠️ tracked in the gate summary: 1.98× locally (~1% headroom) |
| semver-checks | ✅ the baseline is `main`; meaningful from 0.5.0 (on 0.0.x the tool skips all checks, DECISIONS #33) |
| The `unsafe` counter | ✅ 52 occurrences in 7 files (`+7` is the `oom.rs` test allocator, DECISIONS #37); growth without a DECISIONS entry fails the audit |
| Nightly fuzz / soak | ✅ 30 min of fuzzing with a corpus, soak ×5 with a seed from the run number, the suite over 3000 ticks |
| `TooLarge` / `OutOfMemory` instead of `abort` | ✅ unit tests of the limits, the `large_graphs_are_too_large_not_abort` proptest, the `0xFE` fuzz prefix (in the corpus), `tests/oom.rs` with a failing allocator |
| Miri for `pragma_jobs` (1 worker) | ✅ 8 tests under Tree Borrows; under Stacked Borrows ❌ in `crossbeam-epoch`, not in our code (DECISIONS #38; rechecked 2026-10-14 on the latest `crossbeam-epoch` 0.9.21 and nightly 1.101: the same place, `Local::element_of`) |
| The `unsafe` review | ✅ `docs/unsafe.md`: 52 occurrences, no bugs, 2 remarks without code changes |

## M6 — v1.0

- [x] Public API review: `#[non_exhaustive]`, builders, no dependency types exposed
  (M6a, 0.6.0, DECISIONS #42): `pragma_sync` internals moved to `__private` outside semver, no
  `tracing`/`crossbeam` re-exports, `puffin` outside semver, `#[non_exhaustive]` on error variants
- [x] Documentation language: everything in English (M6b, DECISIONS #43, supersedes #11)
- [ ] Complete rustdoc with examples, a guide "How to embed pragma.core in your engine"
- [x] MSRV and versioning policy in the README (M6d, DECISIONS #44)
- [ ] Publication on crates.io

---

## Out of scope (revisited only through DECISIONS.md)

- ECS, rendering, windowing/input, App/plugins, networking: these are the jobs of engines on top of the core
- Process-level crash protection (a crash reporter, minidumps, autosave, a supervisor):
  an application policy; the core only provides context for it (M4), see [reliability.md §4](reliability.md#4-degradation-instead-of-crashing)
- wasm / macOS / mobile: the API does not rule them out, but support is a separate milestone after 1.0
- Fibers/coroutines on their own stacks: a lot of `unsafe` and platform assembly; help-while-wait solves the same problem
- Our own lock-free structures instead of crossbeam (DECISIONS #5)
- Tracy (C++): only behind a feature flag and by a separate decision

## Risks

| Risk | Mitigation |
|---|---|
| Deadlocks in nested `join`/`scope` | help-while-wait by construction; stress tests on 1 worker |
| Lost wakeups when parking | loom models before merging; a timeout safety net when parking |
| `scope` with borrows requires `unsafe` (lifetime erasure) | Isolate it in `pragma_sync`, Miri; study the `std::thread::scope`/rayon approach |
| Background tasks displace the frame | A worker limit for `Background`, a "frame under load" bench |
| Scope creep (ECS "since it's the core anyway") | The "Out of scope" section + DECISIONS #1 |
