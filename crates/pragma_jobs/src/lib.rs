//! L2: work-stealing job system `pragma.core`.
//!
//! Статус: каркас (веха M1 в `docs/roadmap.md`). Сейчас здесь только
//! конфигурация пула; планируемый API описан в `docs/architecture.md` §3.
//!
//! ```
//! use pragma_jobs::PoolConfig;
//!
//! let cfg = PoolConfig::default().with_workers(4).with_name_prefix("pragma-worker");
//! assert_eq!(cfg.validate()?.workers(), 4);
//! # Ok::<(), pragma_jobs::JobsError>(())
//! ```
#![forbid(unsafe_code)]

mod config;
mod error;

pub use config::PoolConfig;
pub use error::JobsError;
