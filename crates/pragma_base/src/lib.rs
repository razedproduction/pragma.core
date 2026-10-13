//! L0-фундамент `pragma.core`: типизированные ошибки, generational-хэндлы,
//! тики симуляции и логирование.
//!
//! Крейт не знает о потоках и не имеет `unsafe` — все верхние слои строятся на нём.
//!
//! ```
//! use pragma_base::{HandlePool, Tick};
//!
//! let mut pool = HandlePool::<&str>::new();
//! let h = pool.insert("job-name")?;
//! assert_eq!(pool.get(h), Some(&"job-name"));
//! pool.remove(h)?;
//! assert_eq!(pool.get(h), None); // устаревший хэндл безопасно возвращает None
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

/// Re-export `tracing`, чтобы все крейты логировали через одну версию.
pub use tracing;
