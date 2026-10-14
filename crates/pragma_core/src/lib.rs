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
