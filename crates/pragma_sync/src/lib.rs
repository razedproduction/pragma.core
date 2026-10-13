//! L1: примитивы синхронизации для планировщика `pragma.core`.
//!
//! Единственный крейт workspace, где допускается `unsafe` (см. `layers.toml`,
//! `DECISIONS.md` #6). Правила: каждый блок с комментарием `// SAFETY:`,
//! тесты под `cargo +nightly miri test -p pragma_sync`, атомарные протоколы —
//! модель-чекинг через `RUSTFLAGS="--cfg loom" cargo test -p pragma_sync --release`.
//!
//! ```
//! use pragma_sync::CountLatch;
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
//! ```
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod latch;

pub use crossbeam_utils::{Backoff, CachePadded};
pub use latch::CountLatch;
