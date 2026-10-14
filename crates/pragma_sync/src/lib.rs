//! L1: synchronization primitives for the `pragma.core` scheduler.
//!
//! The only workspace crate where `unsafe` is allowed (see `layers.toml`,
//! `DECISIONS.md` #6). Rules: every block carries a `// SAFETY:` comment,
//! tests run under `cargo +nightly miri test -p pragma_sync`, and atomic protocols are
//! model-checked via `RUSTFLAGS="--cfg loom" cargo test -p pragma_sync --release --lib --tests`.
//!
//! | Primitive | Purpose |
//! |---|---|
//! | [`CountLatch`] | Wait for N events |
//! | [`AtomicCounter`] | Task counter of a group; wait for zero while helping to run work |
//! | [`current_label`] / [`TaskLabel`] | The thread's task label for the panic hook and crash reports |
//!
//! Scheduler building blocks (`ArrivalCounter`, `Parker`, `scope`, `join_on_stack`,
//! `run_labeled` and others) live in the hidden module `__private`: `pragma_jobs` uses them, they
//! change together with it and are **not covered by semver guarantees** (DECISIONS #42).
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
//! assert!(pending.decrement()); // reached zero
//! pending.wait(); // returns immediately
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

/// Scheduler internals for the `pragma.core` crates. **Not covered by semver guarantees**:
/// they change in any release together with `pragma_jobs`, which depends on this crate with an exact
/// version (DECISIONS #42).
#[doc(hidden)]
pub mod __private {
    pub use crate::arrival::ArrivalCounter;
    pub use crate::join::{JoinLatch, NO_OWNER, StackJob, join_on_stack};
    pub use crate::label::run_labeled;
    pub use crate::parker::{Parker, Unparker};
    pub use crate::scope::{ScopedTask, StackSlot, TaskScope, scope};
    pub use crossbeam_utils::{Backoff, CachePadded};
}
