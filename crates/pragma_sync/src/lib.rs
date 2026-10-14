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
//! | [`ArrivalCounter`] | «Последний из N прибывших продолжает» — счётчики зависимостей графа кадра |
//! | [`Parker`] / [`Unparker`] | Сон простаивающего воркера и адресное пробуждение |
//! | [`scope()`] / [`TaskScope`] / [`ScopedTask`] | Задачи с заимствованием стека (стирание лайфтайма) |
//! | [`StackSlot`] | Задача области без аллокации (замыкание на стеке владельца) |
//! | [`join_on_stack`] / [`StackJob`] | Fork-join без аллокаций и `Arc`: вторая ветвь на стеке с латчем |
//! | [`run_labeled`] / [`current_label`] | Метка задачи потока для panic hook и крэш-репортов |
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

pub use arrival::ArrivalCounter;
pub use counter::AtomicCounter;
pub use crossbeam_utils::{Backoff, CachePadded};
pub use error::SyncError;
pub use join::{JoinLatch, NO_OWNER, StackJob, join_on_stack};
pub use label::{TaskLabel, current_label, run_labeled};
pub use latch::CountLatch;
pub use parker::{Parker, Unparker};
pub use scope::{ScopedTask, StackSlot, TaskScope, scope};
