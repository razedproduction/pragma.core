# pragma.core architecture

## 1. Purpose and boundaries

pragma.core is the layer **under** an engine. It owns the threads, schedules the work of a frame and
runs background I/O. ECS, rendering, physics and audio are built on top and do not start their own pools.

**In scope:** a work-stealing pool of worker threads, tasks (`spawn`/`join`/`scope`/`parallel_for`),
a frame graph with declared resource access, async tasks for I/O, base primitives
(errors, generational handles, ticks, synchronization).

**Deliberately out of scope** (DECISIONS #1): ECS, rendering, windowing and input, App/plugins/the game loop,
networking, fibers/coroutines on their own stacks, wasm/mobile platforms.

## 2. Layers

A crate depends only on crates of a **strictly lower** layer. The rules are in [`layers.toml`](../layers.toml),
checked by `python tools/audit_workspace.py .`.

```
L5  frame_sim       vertical-slice example (examples/, not published), through the facade only
L4  pragma_core     facade: re-exports, prelude, feature flags (jobs, graph, async)
L3  pragma_graph    frame graph            pragma_async   async tasks
L2  pragma_jobs     pool, work-stealing, join/scope/parallel_for
L1  pragma_sync     latches, parking, backoff, task labels   ← unsafe (under Miri + loom)
    pragma_sys      thread names/priorities, affinity        ← unsafe for FFI only (DECISIONS #27)
L0  pragma_base     errors, Handle/HandlePool, Tick
```

External dependencies: pure Rust only ([`deny.toml`](../deny.toml)). All versions live in
`[workspace.dependencies]` of the root `Cargo.toml`.

## 3. Execution model

### 3.1 The pool (`pragma_jobs`, M1)

```
          ┌───────────── Injector (global queue, external spawns) ─────────────┐
          ▼                         ▼                           ▼
   ┌─ Worker 0 ─┐            ┌─ Worker 1 ─┐              ┌─ Worker N ─┐
   │ LIFO deque │◄── steal ──│ LIFO deque │◄──── steal ──│ LIFO deque │
   └────────────┘            └────────────┘              └────────────┘
   the main thread also runs tasks while it waits (help-while-wait)
```

- The default number of workers is `logical cores − 1`: the main thread takes part in the work.
- The local queue is a `crossbeam_deque::Worker` (LIFO for cache locality); stealing is FIFO
  from the tail of another queue. External tasks go through the `Injector`.
- The order of looking for work: own queue → injector → a random victim to steal from → a short spin
  with `Backoff` → parking. Wakeup is targeted: one sleeping worker is woken per new task,
  not all of them at once.
- **A wait never blocks a worker idly.** `join`/`scope`/`JobHandle::join`
  run other tasks while waiting for their own (help-while-wait). This rules out a deadlock of nested
  `join`s with any number of threads, including one.
- **The main thread is a temporary pool participant.** For the duration of `scope`/`join`/`JobHandle::join`
  a thread outside the pool occupies one of 4 external slots: its own LIFO queue, visible to thieves
  (DECISIONS #16). Otherwise its subtasks would go into the FIFO injector, and helping while waiting would take
  the oldest tasks, growing the stack. If the slots are taken, it works through the injector. The slot context
  is cached per thread (the queues go back into the slot and a placeholder takes their place), so entering
  the pool once per frame does not allocate (DECISIONS #18).
- Sleep and wakeup: a worker that found no work first spins with `Backoff`, then for another
  `idle_spin` rounds (`yield` + search, 32 by default, DECISIONS #32) is marked as "searching",
  then registers as sleeping and parks (`Parker`, a 50 ms timeout safety net). A new task
  wakes one sleeper only if there are no searchers. Before stealing from a victim, `is_empty`: crossbeam's
  `steal` enters an epoch even before checking the length.
- **Startup:** `ThreadPool::new` waits until every worker has gone through thread initialization
  (priority/affinity, registration in the crossbeam epoch, the worker context). One-time
  thread allocations do not land in frames, even if there are not enough cores for the workers (DECISIONS #39).
- **`join`** (DECISIONS #30): the second branch is a stack job with a latch
  (`pragma_sync::__private::join_on_stack`). A pool participant (a worker or the main thread in an external slot)
  pushes it into its LIFO queue, runs the first branch and pops the job back: if it was not
  stolen, it runs it itself, with no atomic RMW and no allocations. A stolen one it waits for while helping; with no work it
  announces sleep in the latch and parks on its `Parker` (external slots have their own), and the thief wakes it
  by index. A thread outside the pool without a free slot takes the old path, through a scope.
  After pushing the branch, it checks for sleepers without `fence(SeqCst)` (DECISIONS #40): missing a
  worker that is falling asleep here means losing parallelism, not hanging, since the owner takes the branch back
  itself.
- The `unsafe` for lifetime erasure of `scope` tasks and of stack tasks (`StackSlot`, `StackJob`) lives
  in `pragma_sync` (DECISIONS #15, #30); `pragma_jobs` is `forbid(unsafe_code)`.
- Worker platform settings (M4): `PoolConfig::with_thread_priority` and `with_affinity`
  are applied by the worker itself at startup via `pragma_jobs::sys` (§7); an OS refusal is a warning.
- Metrics (M4): every pool thread (workers and external slots) has its own set of counters on its own
  cache line, written by one thread (`load` + `store`, no RMW). `ThreadPool::stats()` reads them and
  the queue lengths without locking; it does not affect execution or determinism.

API (M1):

```rust
let pool = ThreadPool::new(PoolConfig::default())?;      // a Result, not a panic

let (a, b) = pool.join(|| heavy_a(), || heavy_b())?;     // fork-join, Err if a branch panics

pool.scope(|s| {                                         // tasks may hold references
    for chunk in data.chunks_mut(1024) {
        s.spawn(move || process(chunk));                 // `s` is Copy and can be captured
    }
})?;                                                      // Err if a task panicked

pool.parallel_for(&mut items, Grain::Auto, |item| item.update(dt))?;
let total = pool.parallel_reduce(&items, Grain::Auto, |_, c| sum(c), |a, b| a + b)?;

let handle = pool.spawn(Priority::Background, || bake_navmesh());
let result = handle.join()?;                              // Result<T, JobError>
```

`Grain::Auto` splits the data into ~128 chunks (no fewer than 64 elements each): the split depends only
on the length, not on the number of workers, so `parallel_reduce` is bit-for-bit reproducible (§6).

### 3.2 Priorities

| Priority | Purpose | Policy |
|---|---|---|
| `Frame` | Work of the current frame (graph, `parallel_for`) | Always first |
| `Normal` | Tasks without a frame deadline | After `Frame` |
| `Background` | Baking, decompression, streaming | A separate queue, a limit on the number of simultaneously busy workers |

Background tasks **must not** starve the frame: a limit on busy workers plus a check of the
`Frame` queue between background tasks.

- `Frame` and `Normal` use the workers' local queues and their own injectors; `Background` uses only the
  global queue. The limit (`PoolConfig::with_background_limit`, half of the workers by default)
  counts workers, not tasks: a background task waiting for another background task runs it
  against its own slot, so a limit of 1 does not deadlock.
- Subtasks of `join`/`scope`/`parallel_*` inherit the priority of the current task; outside the pool it is
  `Frame`; for a background task it is `Normal` (otherwise they would hit the limit held by their parent).
- A thread outside the pool does not run background tasks, even while waiting.

### 3.3 The frame graph (`pragma_graph`, M2)

A task declares what it reads and what it writes:

```rust
let mut g = FrameGraph::builder();   // FrameGraph::builder().ordered() for the `ordered` mode
g.task("input", |t| t.writes::<InputState>(), input_system);
g.task("physics", |t| t.reads::<InputState>().writes::<Bodies>().after("input"), physics_step);
g.task("transforms", |t| t.reads::<Bodies>().writes::<Transforms>().after("physics"), propagate);
g.task("audio", |t| t.reads::<Transforms>().after("transforms"), audio_update);
let mut graph = g.build()?;      // checks: duplicates, unknown dependencies, cycles, implicit conflicts
graph.run(&pool, &mut resources)?;
println!("{}", graph.to_mermaid()); // or to_dot(): tasks, access and edges for debugging
```

A system is `Fn(&TaskContext) -> R`, where `R` is `()` or `Result<(), E>`. Resources are taken via
`ctx.read::<T>()` / `ctx.write::<T>()`; for nested parallelism there is `ctx.pool()`.

- Two tasks **conflict** if one writes a resource the other reads or writes.
  A conflict without an explicit order (`before`/`after` or declaration order in the `ordered` mode) is
  a **build error** `GraphError::AmbiguousOrder`, not a race that shows up once in a hundred runs.
  The order of checks and error precedence: `DuplicateTask` → `UnknownDependency` → `Cycle` of explicit
  edges → `AmbiguousOrder` (in `ordered` mode, `Cycle` after adding edges by declaration; an explicit
  order takes precedence over declaration order).
- The build (`build`) happens once: a stable topological sort, transitive reduction of edges,
  successor lists and scratch space for ready tasks; everything is allocated here.
- A topological order with **stable** tie-breaking (by registration order) is the
  basis of reproducibility when debugging (`FrameGraph::order`, `FrameGraph::edges`).
- Execution is allocation-free dataflow (DECISIONS #18): every task has an atomic counter of
  unfinished predecessors, reset at the start of the frame. A task that finishes decrements
  its successors' counters and launches the ones that became ready itself: it continues one in a loop on its
  own thread (chains do not grow the stack) and splits several in half via `join` (which puts the task into
  a stack slot rather than a `Box`). The whole frame is one `scope`, so the main thread enters the pool
  once and runs tasks itself.
- Resources (`Resources`) are one value per type, each in its own `RwLock` slot. At build time the graph
  proves that concurrently running tasks do not conflict, so during a frame the slots
  are taken via `try_read`/`try_write` and never wait; a busy slot is a
  `BorrowConflict` error and an undeclared access is `UndeclaredAccess`, rather than a silent race. No `unsafe`
  is needed. Before the frame it checks that all declared resources are present (`MissingResource`).
- An error or panic of a task does not stop independent branches, but its successors do not
  run in that frame. `run` returns the error of the task with the lowest registration index, so the result
  does not depend on scheduling.
- Groundwork for frame pipelining (simulating N+1 in parallel with preparing the render of N): the graph
  does not own the resources, so it can run over different `Resources` instances
  (an "extract" copy).

### 3.4 Async tasks (`pragma_async`, M3)

```rust
let exec = Executor::new(&pool, AsyncConfig::default())?;   // blocking pool: 4 threads, 1024 tasks
let io = exec.clone();
let mut level = exec.spawn_async(async move {               // the future runs on pool workers (Background)
    let bytes = io.unblock(|| std::fs::read("level.bin")).await??; // synchronous I/O in the blocking pool
    Ok::<_, Box<dyn Error + Send + Sync>>(parse_level(&bytes)?)     // parsing back on a worker
});
// game loop, start of the frame:
if let Some(result) = level.poll_ready() { /* apply */ }
```

- **The executor** (`Executor`) is built on `async-task` (DECISIONS #8): a future becomes a
  `Runnable` that is scheduled into the pool with `Background` priority via
  `pragma_jobs::Spawner`, a lightweight reference to the pool (`Weak`) that queues a task without a handle
  (DECISIONS #24). There is no separate thread pool for async; the background worker limit (§3.2)
  also bounds async tasks, so they do not displace the frame.
- **The I/O model** (DECISIONS #23): there is no reactor (epoll/IOCP), since file I/O on Windows and Linux
  is synchronous. `Executor::unblock(f)` runs the closure in a bounded blocking pool:
  threads are created on demand up to `blocking_threads`, and queued tasks (waiting and
  running) are at most `blocking_capacity`; beyond the limit the call immediately returns a handle with
  `AsyncError::Overloaded`, and the caller does not wait. A blocking task is a `Runnable` too, so
  the result, cancellation and waking the waiting future are `async-task` mechanics, with no channels of our own.
- **Cancellation** is dropping the `AsyncHandle`: the future is dropped on the executor (if it is inside
  `poll` right now, then right after it), and a blocking task not yet taken never starts. `detach()` lets it finish
  without a handle. Stopping the pool cancels tasks at their next scheduling; stopping the
  executor cancels blocking tasks not yet taken (running ones finish, since synchronous I/O cannot be
  interrupted). Waiters get `AsyncError::Cancelled` instead of hanging.
- **A panic** in a future or in an `unblock` closure is `AsyncError::Panicked { task, message }`;
  the worker and the blocking thread stay alive.
- **The "async → frame" bridge**: `AsyncHandle::poll_ready()` is a non-blocking check at the start of
  a frame; the handle is also a `Future` for `.await` from other tasks. A blocking `wait()` is allowed
  only outside pool work (a loading screen between frames); from a worker or a graph frame it
  immediately returns `AsyncError::WouldBlock` (DECISIONS #25), so the rule "no `block_on`
  in frame tasks" is machine-checked.
- **Determinism** with background loading (the `examples/frame_sim` example, DECISIONS #26): the result
  is applied at a tick known in advance; one ready earlier waits in a buffer, a late one is
  waited for by the main thread between frames. The state hash depends neither on the number of workers
  nor on when the load finished.

## 4. Errors, panics and shutdown

- Every crate has one `#[non_exhaustive]` error enum (`thiserror`) with context
  (task name, thread name, handle index).
- A panic in a task is caught (`catch_unwind`) and returned from `join`/`scope`/the handle
  as an error; the worker keeps running (DECISIONS #9).
- The pool's `Drop`: stops accepting tasks → drains or cancels the queue (configurable) →
  wakes and `join`s the threads. Leaked threads after `Drop` are a bug.

## 5. Observability

- `tracing`: the `graph_task` (fields `task`, `priority`, `worker`) and `frame_graph` spans at
  `DEBUG`; `scope`/`parallel_for`/`parallel_reduce` at `TRACE`. Fields are evaluated only if the
  level is enabled; levels are compiled out with the `tracing` features (`max_level_*`,
  `release_max_level_*`). A graph frame does not allocate even with a subscriber (`tests/alloc.rs`).
- Pool metrics: `ThreadPool::stats() -> PoolStats`, per worker and for external threads: tasks
  executed, steals, parks, idle and sleep time, the depth of local and global queues.
- The current task context (DECISIONS #28): `pragma_jobs::labeled(kind, name, f)` sets a label,
  `current_task(|t| …)` reads it (with the chain of the thread's outer labels) without allocations or locks,
  from a panic hook or a crash handler. Labels are set on graph tasks (`graph`), `spawn`/`spawn_named`,
  `scope`/`parallel_*` tasks, `spawn_detached`, async (`spawn_async_named`) and `unblock`; the second
  branch of `join` gets no label (a hot path), and the outer label names the culprit.
- The watchdog (debug only, DECISIONS #29): a pool thread on which a labeled task has been running
  longer than `PoolConfig::with_watchdog` (5 s by default) produces one
  `pragma_jobs::watchdog` warning with the task and thread names. No code in release.
- The `puffin` feature (puffin 0.19.1): scopes for the graph frame, graph tasks and pool operations; the application
  marks frames. Tracy only by a separate decision, because it is C++ (`deny.toml`).

## 6. Determinism

The result must not depend on which thread ran what:

- `parallel_for` with a reduction combines partial results in a **fixed chunk order**,
  not in completion order.
- The frame graph: an explicit order for conflicting tasks; a stable topological sort.
- The core does not use `thread_rng`, `Instant::now()` in logic, or a randomly seeded `HashMap` where
  the iteration order affects the result.
- The determinism test: since M3, `examples/frame_sim` in CI (the hash of every tick on 1 and 4 workers,
  with an instant and a delayed background load); since M5, a mandatory suite of scenarios.

## 7. Platforms

Windows and Linux, x86_64 (DECISIONS #2). Platform code (thread names, thread priorities,
affinity) lives only in the `pragma_sys` crate (L1, DECISIONS #27; re-exported in `pragma_jobs` as
`pragma_jobs::sys`) behind `cfg(target_os)` and uses system FFI (`windows-sys`, `libc`).
Every call has a `// SAFETY:`; Miri does not execute FFI, so the crate is checked by tests on a
real OS in CI on both platforms. On other OSes the functions return `SysError::Unsupported`.
Limitations: on Windows, affinity covers the cores of the first group (`< 64`); on Linux, raising the priority
requires `CAP_SYS_NICE`, and without privileges `nice` cannot be moved back after `Low` either. If wasm is needed later,
help-while-wait allows adding a single-threaded mode (the calling thread runs all tasks) without changing the public API.
That is a separate milestone.
