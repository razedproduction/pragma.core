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
//! | `sys` | `pragma_sys` (через `pragma_jobs::sys`) | `jobs` |
//! | `puffin` | `puffin` 0.19 (re-export той же версии) | `puffin` |
//!
//! Feature `puffin` добавляет области профайлера на `scope`/`parallel_*` и на задачи графа;
//! кадр размечает приложение (`puffin::GlobalProfiler::lock().new_frame()`), запись включает
//! `puffin::set_scopes_on(true)`.
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

/// Платформенный слой: имена, приоритеты потоков, привязка к ядрам.
#[cfg(feature = "jobs")]
pub use pragma_jobs::sys;

/// Версия `puffin`, с которой собрано ядро: кадры размечайте через неё.
#[cfg(feature = "puffin")]
pub use puffin;

#[cfg(feature = "graph")]
pub use pragma_graph as graph;

#[cfg(feature = "async")]
pub use pragma_async as tasks;

/// Часто используемые типы одним импортом.
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
