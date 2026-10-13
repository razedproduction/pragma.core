//! # pragma.core
//!
//! Надёжное и высокопроизводительное многопоточное ядро на чистом Rust для
//! игровых движков и фреймворков.
//!
//! Фасад: re-export подсистем за feature-флагами и `prelude`.
//!
//! | Модуль | Крейт | Feature |
//! |---|---|---|
//! | [`base`] | `pragma_base` | всегда |
//! | [`sync`] | `pragma_sync` | всегда |
//! | `jobs` | `pragma_jobs` | `jobs` |
//! | `graph` | `pragma_graph` | `graph` |
//! | `tasks` | `pragma_async` | `async` |
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
pub use pragma_sync as sync;

#[cfg(feature = "jobs")]
pub use pragma_jobs as jobs;

#[cfg(feature = "graph")]
pub use pragma_graph as graph;

#[cfg(feature = "async")]
pub use pragma_async as tasks;

/// Часто используемые типы одним импортом.
pub mod prelude {
    pub use pragma_base::{Handle, HandlePool, Tick};
    pub use pragma_sync::CountLatch;

    #[cfg(feature = "jobs")]
    pub use pragma_jobs::{Grain, JobError, JobHandle, PoolConfig, Priority, Scope, ThreadPool};

    #[cfg(feature = "graph")]
    pub use pragma_graph::{FrameGraph, GraphError, Resources, TaskContext};
}
