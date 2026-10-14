# Reliability rules

Reliability here is a set of properties that are **machine-checked in CI**. A property that
cannot be checked automatically degrades over time.

## 1. Panics

Forbidden by lints in runtime code (`[workspace.lints.clippy]` in the root `Cargo.toml`):
`unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `todo`, `unimplemented`, `dbg_macro`.
Allowed in tests (`clippy.toml`).

- Instead of `v[i]` we use `v.get(i)` and handle `None`.
- A violated internal invariant (a core bug) is a `debug_assert!` plus safe behavior in release.
- `arithmetic_side_effects` is `warn` (an error in CI because of `-D warnings`): we use
  `checked_*`/`saturating_*`/`wrapping_*` deliberately.
- A poisoned mutex is no reason to panic: `unwrap_or_else(PoisonError::into_inner)`, if the
  protected data stays consistent.
- **An allocation whose size the input sets goes only through `try_reserve` or under a limit**
  (DECISIONS #37). Running out of memory is an error with context, not an `abort` in `handle_alloc_error`.
  Currently: the graph has `MAX_TASKS` / `MAX_EDGES` + `try_reserve` → `GraphError::TooLarge`;
  `HandlePool` has `try_reserve` → `BaseError::OutOfMemory`; the blocking queue has `blocking_capacity`
  → `Overloaded`; the number of workers is ≤ 512, and `PoolStats` and the watchdog cells are sized by it; a thread name
  for FFI is at most 256 UTF-16 units (Windows) / 15 bytes (Linux). Fixed-size allocations per call
  (the task `Box` in `spawn`) do not fall under the rule: the queue grows at the
  caller's discretion, as with `std::sync::mpsc`.

## 2. Errors

- One public `enum Error` per crate, `#[non_exhaustive]`, via `thiserror`.
  Exception: `pragma_jobs` has `JobsError` (the pool) and `JobError` (a task outcome), DECISIONS #13.
- An error carries context: the task name, the thread name, the handle's index and generation.
- `anyhow` only in examples and tools.
- Errors that repeat every frame are logged **once** (deduplicated by key).

## 3. `unsafe`

- `#![forbid(unsafe_code)]` in every crate except `pragma_sync` and `pragma_sys`
  (`layers.toml: unsafe_allowed`). `pragma_sys` contains only system FFI calls (DECISIONS #27);
  Miri does not execute them, so the crate is checked by tests on Windows and Linux in CI.
- In `pragma_sync`: `#![deny(unsafe_op_in_unsafe_fn)]`, `clippy::undocumented_unsafe_blocks = deny`,
  a `// SAFETY:` comment on every block explaining why the invariant holds.
- The `pragma_sync` tests run under Miri (`cargo +nightly miri test -p pragma_sync`), and the
  `pragma_jobs/tests/miri.rs` suite (a 1-worker pool) runs under Miri with Tree Borrows (DECISIONS #38).
  A review of every `unsafe` occurrence (invariant → who guarantees it → how it is checked) is in
  [unsafe.md](unsafe.md).
- Every atomic protocol (parking, latches, graph counters, the `join_on_stack` latch) gets a loom model:
  `RUSTFLAGS="--cfg loom" cargo test -p pragma_sync --release --lib --tests` (the models are in
  `crates/pragma_sync/tests/loom.rs`). loom models `SeqCst` as `AcqRel`, so protocols
  are built on RMW operations rather than on "Dekker via a SeqCst load" (see `AtomicCounter`).
- New `unsafe` outside `pragma_sync` only through an entry in `DECISIONS.md`. The only
  exception is the counting allocator in the `pragma_graph/tests/alloc.rs` test (DECISIONS #19).
- **The `unsafe` counter** (DECISIONS #36): `[unsafe_budget]` in `layers.toml` is the exact number
  of keyword occurrences per file and the decisions that allowed them. The audit
  (`tools/audit_workspace.py`) fails on any mismatch, and in a PR a file's budget growth without a
  new decision number is checked against the base revision and fails CI.

## 4. Degradation instead of crashing

| Failure | Behavior |
|---|---|
| A panic in a task | Caught; an error from `join`/`scope`/the handle; the worker stays alive |
| The OS refused to create a thread | `ThreadPool::new` stops and `join`s the workers already started and returns `JobsError::SpawnFailed` with the thread name; no half-built pool is returned |
| A cycle or an ambiguous order in the graph | A `build()` error with the task names, before the first frame |
| An error or panic of a graph task | Its successors are skipped in the frame, independent branches finish; `run` returns the error of the task with the lowest index |
| A graph task took an undeclared or busy resource | `UndeclaredAccess` / `BorrowConflict` instead of a race; a slot poisoned by a panic stays accessible |
| Dropping an async task handle | Cancellation without leaks: the future is dropped on the executor, a blocking task not yet taken never starts |
| A panic in a future or in `unblock` | `AsyncError::Panicked`; the worker and the blocking thread stay alive |
| The blocking pool is full | `AsyncError::Overloaded` immediately; the caller (the frame) does not wait |
| Stopping the pool or the executor with live async tasks | The tasks are cancelled, waiters get `Cancelled` instead of hanging |
| `wait()` on an async handle from a worker or a graph frame | `AsyncError::WouldBlock` instead of a deadlock (DECISIONS #25) |
| A task hangs | The watchdog in debug builds: one `pragma_jobs::watchdog` warning with the task and thread names (`PoolConfig::with_watchdog`, 5 s); no code in release |
| The OS refused a thread priority or pinning | A warning in the log; the worker runs without the setting (`SysError` from `pragma_sys`) |
| The graph is too large (more than `MAX_TASKS` tasks or `MAX_EDGES` edges) or memory for the plan could not be allocated | `GraphError::TooLarge { tasks, reason }` from `build()`, not `abort` (test, proptest, fuzz) |
| The allocator failed while growing a `HandlePool` | `BaseError::OutOfMemory { slots }`, the pool is intact; `remove` does not allocate (`tests/oom.rs`) |

### What the core does not guarantee

These in-process failures are not caught. Reacting to them (a crash reporter, minidumps,
autosave, restart by a supervisor) is the responsibility of the application or the engine, not the core.

| Failure | Why |
|---|---|
| A panic with `panic = "abort"` in the application profile | `catch_unwind` does not work; the process terminates (DECISIONS #9) |
| A panic while waiting in `scope`/`join` | A deliberate `abort`: otherwise the data borrowed by the tasks would dangle (DECISIONS #15) |
| Stack overflow | Rust terminates the process; it cannot be caught |
| Out of memory outside bounded allocations (§1) | The standard allocator calls `handle_alloc_error` → `abort`; input-sized allocations in the core go through `try_reserve`/limits |
| A memory access fault, an OS signal | Outside the Rust model; does not happen in the absence of `unsafe` bugs in `pragma_sync` |

What the core gives the application for these cases: the context of the current task on the thread,
`pragma_jobs::current_task(|t| …)`, without allocations or locks (DECISIONS #28); it can be
read from a panic hook or a crash handler to name the culprit in the report (test
`pragma_core/tests/crash_context.rs`, allocation-free: `pragma_graph/tests/alloc.rs`); and determinism
(§5), so that a crash can be reproduced by replaying the scenario.

## 5. Determinism

Desirable now and **mandatory by v1.0** (DECISIONS #3). Details are in
[architecture.md §6](architecture.md#6-determinism). The test: the same scenario on 1, 2 and N
workers gives the same hash.

## 6. The test pyramid

| Level | Tool | What it covers | Since milestone |
|---|---|---|---|
| Unit | `cargo test` | Module logic | M0 |
| Property | `proptest` | Handles, graphs (random DAGs → a valid order), `parallel_for` = sequential | M0 |
| Doctest | rustdoc | The `//!` example of every crate compiles and works | M0 |
| Model checking | `loom` | `pragma_sync` atomic protocols | M1 |
| UB | Miri | `pragma_sync`, as well as `pragma_jobs` tests in single-threaded mode | M1 |
| Stress / soak | Long `#[ignore]` tests | Millions of tasks, nested `join`, random panics; async tasks with random cancellation (M3) | M1 |
| Fuzz | `cargo-fuzz` (`fuzz/`, a separate workspace) | The graph builder on arbitrary descriptions; the same oracle in proptest | M2 |
| Allocations | A counting global allocator (`pragma_graph/tests/alloc.rs`) | An allocation-free graph frame (since M4 also with a `tracing` subscriber and reading the label from a panic hook) | M2 |
| Determinism | State hash (`frame_sim --suite`, `frame_sim/tests/suite.rs`) | 5 scenarios × 1/2/N workers × instant/delayed load; `idle_spin`, `PoolStats`, watchdog, a `tracing` subscriber do not change the hash (DECISIONS #35) | M3; the mandatory suite since M5 |
| Performance | `criterion` + `tools/bench_gate.py` | spawn/join, `parallel_for`, graph, `frame_sim` ms/tick; comparison with `rayon`; an A/B gate against `main` (DECISIONS #34) | M1; the regression gate since M5 |
| API | `cargo-semver-checks` | A breaking change without a version bump (the baseline is `main`, DECISIONS #33) | M5 |
| `unsafe` budget | `tools/audit_workspace.py` | The exact `unsafe` count per file; growth only with a DECISIONS entry (#36) | M5 |

## 7. CI

[`.github/workflows/ci.yml`](../.github/workflows/ci.yml):

1. `cargo fmt --check`
2. `cargo clippy --all-targets --all-features -D warnings` and `--no-default-features`
3. `cargo test --workspace --all-features` (Windows + Linux)
4. `cargo doc` without warnings
5. MSRV 1.88
6. `cargo deny check`: licenses, advisories, banned C crates
7. `tools/audit_workspace.py`: purity, layers and the `unsafe` budget (in a PR, against the base revision)
8. Miri and loom for `pragma_sync`; Miri for `pragma_jobs/tests/miri.rs` (Tree Borrows)
9. `cargo hack clippy --each-feature`: every facade feature builds on its own
10. Graph builder fuzzing, 60 s (`cargo +nightly fuzz run graph_build`, Linux)
11. The determinism suite: `cargo run -p frame_sim --release -- --suite --ticks 200` (Windows + Linux;
    the debug variant is `frame_sim/tests/suite.rs` in step 3)
12. The watchdog does not fire in release: `cargo test -p pragma_jobs --release --test watchdog` (Windows + Linux)
13. `cargo semver-checks --workspace --baseline-rev <base>` (PR, Windows + Linux)
14. The `tools/bench_gate.py` bench gate (PR, Windows + Linux): an A/B against `main`; a regression > 10%
    beyond the reference noise fails the PR; the methodology is in [perf.md §6](perf.md#6-bench-gate-in-ci)

Nightly ([`.github/workflows/nightly.yml`](../.github/workflows/nightly.yml)), Windows and Linux:
soak, i.e. the `pragma_jobs` and `pragma_async` stress tests (`--ignored`) 5 times longer with a new seed
every night (`PRAGMA_SOAK_ROUNDS`, `PRAGMA_SOAK_SEED`), and the determinism suite over 3000 ticks;
Linux: 30 minutes of graph builder fuzzing with a corpus that survives across nights (`actions/cache`), findings
as an artifact.

## 8. API stability

- Growing enums/structs are marked `#[non_exhaustive]`; constructors and builders instead of public fields.
- Dependency types (`crossbeam`, `async-task`) do not leak into the public API, except for deliberate re-exports.
- Deprecation: `#[deprecated(since, note)]` for at least one minor release.
- Before v1.0, breaking changes are allowed in minor versions (`0.x`), but each one goes into the CHANGELOG.
