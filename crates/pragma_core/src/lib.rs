//! # pragma.core
//!
//! A solid, high-performance multithreaded core in pure Rust for
//! game engines and frameworks.
//!
//! The facade: re-exports of the subsystems behind feature flags, and a `prelude`.
//!
//! | Module | Crate | Feature |
//! |---|---|---|
//! | [`base`] | `pragma_base` | always |
//! | [`sync`] | `pragma_sync` | always |
//! | `jobs` | `pragma_jobs` | `jobs` |
//! | `graph` | `pragma_graph` | `graph` |
//! | `tasks` | `pragma_async` | `async` |
//! | `sys` | `pragma_sys` (via `pragma_jobs::sys`) | `jobs` |
//! | `puffin` | `puffin` 0.19 (a re-export of the same version) | `puffin` |
//!
//! The `puffin` feature adds profiler scopes to `scope`/`parallel_*` and to graph tasks;
//! the application marks frames (`puffin::GlobalProfiler::lock().new_frame()`), and recording is
//! enabled with `puffin::set_scopes_on(true)`.
//!
//! ## Semver guarantees
//!
//! The public API of all modules follows semver, except for:
//! - the `puffin` feature and the [`puffin`](https://docs.rs/puffin) re-export: the puffin version
//!   is bumped in a minor release (puffin is not 1.0 yet);
//! - items hidden from the documentation (`#[doc(hidden)]`, for example `pragma_sync::__private`):
//!   these are internals for the core's own crates.
//!
//! The core logs through `tracing` 0.1: the application installs a subscriber through its own
//! `tracing` dependency.
//!
//! ```
//! use pragma_core::prelude::*;
//!
//! let mut pool = HandlePool::new();
//! let h = pool.insert(42_u32)?;
//! assert_eq!(pool.get(h), Some(&42));
//! # Ok::<(), pragma_core::base::BaseError>(())
//! ```
//!
//! ## Integrating into an engine
//!
//! The core is a library, not a framework: it has no main loop and no global state, and the
//! engine decides how the pieces are wired. These are the decisions to make, with the defaults the
//! core is designed and tested for. The design behind them is in
//! [`docs/architecture.md`](https://github.com/razedproduction/pragma.core/blob/main/docs/architecture.md)
//! §3–6; [`examples/frame_sim`](https://github.com/razedproduction/pragma.core/tree/main/examples/frame_sim)
//! is a complete headless engine loop built this way.
//!
//! 1. **Pool ownership and the number of workers.** Create **one** [`ThreadPool`](jobs::ThreadPool)
//!    per process at startup, keep it in the engine's top-level struct and lend `&ThreadPool` to
//!    subsystems. Several pools oversubscribe the cores and cannot help each other while waiting.
//!    The default config has cores − 1 workers because the thread that calls `FrameGraph::run`,
//!    `scope` or `join` runs tasks itself while it waits; if the engine keeps another busy thread
//!    of its own (a render thread), subtract it too with
//!    [`PoolConfig::with_workers`](jobs::PoolConfig::with_workers). Thread priority and core pinning
//!    are optional: a refusal by the OS is a warning, not an error.
//! 2. **The graph in the game loop.** Build the [`FrameGraph`](graph::FrameGraph) once at startup
//!    (or when the set of systems changes): ordering mistakes surface there as a
//!    [`GraphError`](graph::GraphError) before the first frame. The engine owns the
//!    [`Resources`](graph::Resources); between frames it changes them directly through
//!    [`get_mut`](graph::Resources::get_mut), during a frame tasks reach them only through
//!    [`TaskContext`](graph::TaskContext) and only as declared. A frame that returns an error leaves
//!    the graph usable: log it and run the next frame. The graph does not own the resources, so it
//!    can also run over an "extract" copy of the state for rendering.
//! 3. **Async results enter the frame through `poll_ready`, not `wait`.** At the start of a frame,
//!    before `run`, check the pending [`AsyncHandle`](tasks::AsyncHandle)s with
//!    [`poll_ready`](tasks::AsyncHandle::poll_ready) and move finished results into the resources.
//!    A blocking [`wait`](tasks::AsyncHandle::wait) belongs to a loading screen between frames; from
//!    pool work it returns `AsyncError::WouldBlock` instead of stalling the frame (DECISIONS #25).
//!    Never block inside a task: file reads and other synchronous I/O go through
//!    [`Executor::unblock`](tasks::Executor::unblock).
//! 4. **Background work stays in `Background`.** Streaming, baking and decompression are
//!    `spawn(Priority::Background, ..)` or `spawn_async` (always background). At most
//!    [`with_background_limit`](jobs::PoolConfig::with_background_limit) workers (half by default)
//!    run them at once, and the main thread never picks them up while it waits, so a frame always
//!    has workers. Frame work needs no explicit priority: graph tasks and their `scope`/`join`/
//!    `parallel_*` subtasks are `Frame`.
//! 5. **A panic hook that names the task.** Install a hook at startup that reads
//!    [`current_task`](jobs::current_task) (no allocations, no locks) and writes the label chain
//!    into the crash report; the hook runs on the panicking thread before unwinding. Name your own
//!    long-running work with [`labeled`](jobs::labeled) and
//!    [`spawn_named`](jobs::ThreadPool::spawn_named) so the report says more than `spawn`.
//! 6. **`panic = "unwind"` or `"abort"`.** With unwinding (the default), a panic in a task is caught
//!    and comes back as `JobError::Panicked`, `GraphError::TaskPanicked` or `AsyncError::Panicked`;
//!    the worker survives and the engine chooses whether to skip the frame or shut down. With
//!    `panic = "abort"` nothing is caught: the process ends, and the panic hook (item 5) is the only
//!    place to report the culprit. Either way, a panic in the thread that is waiting inside
//!    `scope`/`join` aborts on purpose, because the tasks borrow its stack (DECISIONS #15).
//! 7. **Shutdown order.** Stop starting frames, then drop (cancel) or
//!    [`detach`](tasks::AsyncHandle::detach) the pending async handles, drop the
//!    [`Executor`](tasks::Executor) (it waits for `unblock` closures that are already running, so
//!    keep them short), then the graph and the resources, and the pool last. Dropping the pool
//!    drains its queues, or drops them with [`ShutdownPolicy::Cancel`](jobs::ShutdownPolicy::Cancel),
//!    and joins every worker. Struct fields drop in declaration order, so declaring them in this
//!    order is enough. Never drop the pool from inside one of its own tasks.
//! 8. **Determinism.** The core gives the same result on any number of workers: the
//!    [`Grain`](jobs::Grain) split ignores the worker count, `parallel_reduce` combines in chunk
//!    order, and the graph orders every conflict. Keep the engine's side the same way: count time
//!    in [`Tick`](base::Tick)s rather than `Instant`, do not let results depend on
//!    `current_worker_index` or on completion order, and apply an async result at a tick fixed in
//!    advance rather than in whichever frame it happens to finish (DECISIONS #26). Then check it the
//!    way `frame_sim --verify` does: the same scenario on 1 and N workers must give the same state
//!    hash.
//!
//! The skeleton of such an engine, with the fields in shutdown order:
//!
//! ```
//! use pragma_core::graph::{FrameGraph, GraphError, Resources};
//! use pragma_core::jobs::{PoolConfig, ThreadPool};
//! use pragma_core::tasks::{AsyncConfig, AsyncHandle, Executor};
//!
//! struct Level(Vec<u8>);
//! struct Bodies(Vec<f32>);
//!
//! struct Engine {
//!     loading: Option<AsyncHandle<Vec<u8>>>, // dropped first: cancels the load
//!     exec: Executor,
//!     graph: FrameGraph,
//!     world: Resources,
//!     pool: ThreadPool, // dropped last: joins the workers
//! }
//!
//! impl Engine {
//!     fn new() -> Result<Self, Box<dyn std::error::Error>> {
//!         std::panic::set_hook(Box::new(|info| {
//!             pragma_core::jobs::current_task(|task| match task {
//!                 Some(task) => eprintln!("panic in {task}: {info}"),
//!                 None => eprintln!("panic: {info}"),
//!             });
//!         }));
//!         let pool = ThreadPool::new(PoolConfig::default().with_name_prefix("engine"))?;
//!         let exec = Executor::new(&pool, AsyncConfig::default())?;
//!
//!         let mut g = FrameGraph::builder();
//!         g.task("physics", |t| t.reads::<Level>().writes::<Bodies>(), |ctx| -> Result<(), GraphError> {
//!             let gravity = f32::from(ctx.read::<Level>()?.0.first().copied().unwrap_or(0));
//!             let mut bodies = ctx.write::<Bodies>()?;
//!             ctx.pool().parallel_for(&mut bodies.0, Default::default(), |v| *v -= gravity)?;
//!             Ok(())
//!         });
//!         let graph = g.build()?; // ordering mistakes fail here, at startup
//!
//!         let mut world = Resources::new();
//!         world.insert(Level(Vec::new()));
//!         world.insert(Bodies(vec![0.0; 10_000]));
//!         // std::fs::read("level.bin") in a real engine
//!         let loading = Some(exec.unblock(|| vec![1_u8, 2, 3]));
//!         Ok(Self { loading, exec, graph, world, pool })
//!     }
//!
//!     fn frame(&mut self) -> Result<(), GraphError> {
//!         // 1. Async results enter the frame between frames, without blocking.
//!         if let Some(result) = self.loading.as_mut().and_then(AsyncHandle::poll_ready) {
//!             self.loading = None;
//!             match result {
//!                 Ok(bytes) => {
//!                     self.world.insert(Level(bytes));
//!                 }
//!                 Err(err) => eprintln!("level failed to load: {err}"),
//!             }
//!         }
//!         // 2. The frame itself: the calling thread works too.
//!         self.graph.run(&self.pool, &mut self.world)
//!     }
//! }
//!
//! let mut engine = Engine::new()?;
//! for _ in 0..3 {
//!     if let Err(err) = engine.frame() {
//!         eprintln!("frame failed: {err}"); // the graph stays usable
//!     }
//! }
//! while engine.loading.is_some() {
//!     engine.frame()?;
//! }
//! assert!(engine.world.get::<Level>().is_some_and(|l| l.0 == [1, 2, 3]));
//! drop(engine); // fields in declaration order: handles, executor, graph, resources, pool
//! # let _ = std::panic::take_hook();
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![forbid(unsafe_code)]

pub use pragma_base as base;

/// Synchronization primitives (the stable part of `pragma_sync`).
pub mod sync {
    pub use pragma_sync::{AtomicCounter, CountLatch, SyncError, TaskLabel, current_label};
}

#[cfg(feature = "jobs")]
pub use pragma_jobs as jobs;

/// The platform layer: thread names, priorities, core pinning.
#[cfg(feature = "jobs")]
pub use pragma_jobs::sys;

/// The `puffin` version the core is built with: mark frames through it.
#[cfg(feature = "puffin")]
pub use puffin;

#[cfg(feature = "graph")]
pub use pragma_graph as graph;

#[cfg(feature = "async")]
pub use pragma_async as tasks;

/// Frequently used types in a single import.
///
/// ```
/// use pragma_core::prelude::*;
///
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
/// let mut values = vec![1_u64; 1024];
/// pool.parallel_for(&mut values, Grain::Auto, |v| *v *= 3)?;
/// let total = pool.spawn(Priority::Normal, || 6_u64).join()?;
/// assert_eq!(values.iter().sum::<u64>() + total, 3078);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub mod prelude {
    pub use pragma_base::{Handle, HandlePool, Tick};
    pub use pragma_sync::CountLatch;

    #[cfg(feature = "jobs")]
    pub use pragma_jobs::{
        Grain, JobError, JobHandle, PoolConfig, PoolStats, Priority, Scope, TaskLabel, ThreadPool,
        current_task, labeled,
    };

    #[cfg(feature = "graph")]
    pub use pragma_graph::{FrameGraph, GraphError, Resources, TaskContext};

    #[cfg(feature = "async")]
    pub use pragma_async::{AsyncConfig, AsyncError, AsyncHandle, Executor};
}
