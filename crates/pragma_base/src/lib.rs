//! The L0 foundation of `pragma.core`: typed errors, generational handles and
//! simulation ticks.
//!
//! The crate knows nothing about threads and contains no `unsafe`; every upper layer builds on it.
//!
//! ```
//! use pragma_base::{HandlePool, Tick};
//!
//! let mut pool = HandlePool::<&str>::new();
//! let h = pool.insert("job-name")?;
//! assert_eq!(pool.get(h), Some(&"job-name"));
//! pool.remove(h)?;
//! assert_eq!(pool.get(h), None); // a stale handle safely yields None
//!
//! let t = Tick::ZERO.next()?;
//! assert_eq!(t.get(), 1);
//! # Ok::<(), pragma_base::BaseError>(())
//! ```
#![forbid(unsafe_code)]

mod error;
mod handle;
mod tick;

pub use error::BaseError;
pub use handle::{Handle, HandlePool};
pub use tick::Tick;
