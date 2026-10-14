//! L1: the platform layer of `pragma.core`: thread names, priorities and core pinning.
//!
//! The second crate (after `pragma_sync`) where `unsafe` is allowed (`layers.toml`, DECISIONS #27):
//! it contains only system FFI calls (`windows-sys` on Windows, `libc` on Linux), each
//! with a `// SAFETY:` comment. Other crates use the safe API from here; in
//! `pragma_jobs` it is available as `pragma_jobs::sys`.
//!
//! All functions act on the **current** thread. On other platforms they return
//! [`SysError::Unsupported`] instead of panicking: platform tuning is an optimization the
//! core works without.
//!
//! ```
//! use pragma_sys::{ThreadPriority, set_current_thread_name, set_current_thread_priority};
//!
//! std::thread::spawn(|| {
//!     set_current_thread_name("asset-loader")?;
//!     // Lowering the priority needs no admin rights; raising it may return
//!     // `SysError::PermissionDenied` (Linux without CAP_SYS_NICE).
//!     set_current_thread_priority(ThreadPriority::Low)?;
//!     Ok::<(), pragma_sys::SysError>(())
//! })
//! .join()
//! .map_err(|_| "thread panicked")??;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod error;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod imp;
#[cfg(windows)]
#[path = "windows.rs"]
mod imp;
#[cfg(not(any(windows, target_os = "linux")))]
#[path = "fallback.rs"]
mod imp;

pub use error::SysError;

/// Thread priority relative to the other threads of the process.
///
/// | | Windows | Linux (nice) |
/// |---|---|---|
/// | `Low` | `THREAD_PRIORITY_BELOW_NORMAL` | 10 |
/// | `Normal` | `THREAD_PRIORITY_NORMAL` | 0 |
/// | `High` | `THREAD_PRIORITY_ABOVE_NORMAL` | −5 (requires `CAP_SYS_NICE`) |
///
/// On Linux without privileges `nice` cannot be moved back to a lower value either: after `Low`,
/// a call with `Normal` returns [`SysError::PermissionDenied`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ThreadPriority {
    /// Below normal: background work that must not disturb the frame.
    Low,
    /// The regular OS priority.
    #[default]
    Normal,
    /// Above normal: frame threads.
    High,
}

/// Sets the name of the current thread, as seen in a debugger and a profiler.
///
/// `pragma_jobs` pool threads are already named via `std::thread::Builder`; this function is for
/// threads created without a `Builder` (for example the main one). The name is truncated at a
/// character boundary: to 15 bytes on Linux (a kernel limit), to 256 UTF-16 units on Windows.
pub fn set_current_thread_name(name: &str) -> Result<(), SysError> {
    if name.contains('\0') {
        return Err(SysError::InvalidName);
    }
    imp::set_current_thread_name(name)
}

/// Sets the priority of the current thread.
pub fn set_current_thread_priority(priority: ThreadPriority) -> Result<(), SysError> {
    imp::set_current_thread_priority(priority)
}

/// The priority of the current thread, mapped to [`ThreadPriority`] (any value below
/// normal is `Low`, any value above is `High`).
pub fn current_thread_priority() -> Result<ThreadPriority, SysError> {
    imp::current_thread_priority()
}

/// Pins the current thread to logical core `core` (zero-based).
///
/// On Windows, cores of the first processor group are supported (`core < 64`); on Linux,
/// `core < 1024` (`CPU_SETSIZE`); a core out of range is [`SysError::InvalidCore`].
pub fn pin_current_thread(core: usize) -> Result<(), SysError> {
    imp::pin_current_thread(core)
}

/// The logical core the thread is running on right now (`None` if the OS does not report it).
///
/// Without pinning, the value may change right after the call; it is good for diagnostics and
/// for checking [`pin_current_thread`].
#[must_use]
pub fn current_core() -> Option<usize> {
    imp::current_core()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each check runs on its own thread: priority and pinning change the current thread.
    fn on_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::spawn(f).join().unwrap()
    }

    #[test]
    fn name_with_nul_is_rejected() {
        assert_eq!(set_current_thread_name("a\0b"), Err(SysError::InvalidName));
    }

    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn name_is_applied() {
        on_thread(|| {
            set_current_thread_name("pragma-sys-test-long-name").unwrap();
        });
    }

    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn lower_priority_reads_back() {
        let read = on_thread(|| {
            set_current_thread_priority(ThreadPriority::Low).unwrap();
            current_thread_priority().unwrap()
        });
        assert_eq!(read, ThreadPriority::Low);
    }

    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn default_priority_is_normal() {
        assert_eq!(
            on_thread(current_thread_priority).unwrap(),
            ThreadPriority::Normal
        );
    }

    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn pinned_thread_runs_on_its_core() {
        let core = on_thread(|| {
            pin_current_thread(0).unwrap();
            // The move to the target core happens at the next scheduling point.
            std::thread::yield_now();
            current_core()
        });
        assert_eq!(core, Some(0));
    }

    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn out_of_range_core_is_an_error() {
        let result = on_thread(|| pin_current_thread(1 << 20));
        assert!(matches!(result, Err(SysError::InvalidCore { .. })));
    }
}
