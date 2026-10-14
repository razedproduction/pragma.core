//! L1: примитивы синхронизации для планировщика `pragma.core`.
//!
//! Единственный крейт workspace, где допускается `unsafe` (см. `layers.toml`,
//! `DECISIONS.md` #6). Правила: каждый блок с комментарием `// SAFETY:`,
//! тесты под `cargo +nightly miri test -p pragma_sync`, атомарные протоколы —
//! модель-чекинг через `RUSTFLAGS="--cfg loom" cargo test -p pragma_sync --release --lib --tests`.
//!
//! | Примитив | Для чего |
//! |---|---|
//! | [`CountLatch`] | Ждать N событий |
//! | [`AtomicCounter`] | Счётчик задач группы; ждать нуля, помогая исполнять работу |
//! | [`current_label`] / [`TaskLabel`] | Метка задачи потока для panic hook и крэш-репортов |
//!
//! Строительные блоки планировщика (`ArrivalCounter`, `Parker`, `scope`, `join_on_stack`,
//! `run_labeled` и др.) лежат в скрытом модуле `__private`: их использует `pragma_jobs`, они
//! меняются вместе с ним и **не входят в гарантии semver** (DECISIONS #42).
//!
//! ```
//! use pragma_sync::{AtomicCounter, CountLatch};
//! use std::sync::Arc;
//!
//! let latch = Arc::new(CountLatch::new(2));
//! let workers: Vec<_> = (0..2)
//!     .map(|_| {
//!         let latch = Arc::clone(&latch);
//!         std::thread::spawn(move || latch.count_down())
//!     })
//!     .collect();
//! latch.wait();
//! assert!(latch.is_released());
//! # for w in workers { w.join().unwrap(); }
//!
//! let pending = AtomicCounter::new(0);
//! pending.increment()?;
//! assert!(pending.decrement()); // стал нулём
//! pending.wait(); // возвращается сразу
//! # Ok::<(), pragma_sync::SyncError>(())
//! ```
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod arrival;
mod counter;
mod error;
mod join;
mod label;
mod latch;
mod parker;
mod primitives;
mod scope;

pub use counter::AtomicCounter;
pub use error::SyncError;
pub use label::{TaskLabel, current_label};
pub use latch::CountLatch;

/// Внутренности планировщика для крейтов `pragma.core`. **Не входят в гарантии semver**:
/// меняются в любом релизе вместе с `pragma_jobs`, который зависит от этого крейта точной
/// версией (DECISIONS #42).
#[doc(hidden)]
pub mod __private {
    pub use crate::arrival::ArrivalCounter;
    pub use crate::join::{JoinLatch, NO_OWNER, StackJob, join_on_stack};
    pub use crate::label::run_labeled;
    pub use crate::parker::{Parker, Unparker};
    pub use crate::scope::{ScopedTask, StackSlot, TaskScope, scope};
    pub use crossbeam_utils::{Backoff, CachePadded};
}
