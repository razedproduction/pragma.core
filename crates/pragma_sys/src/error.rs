/// Errors of the platform layer.
///
/// Platform tuning is optional, so most callers log the error and carry on:
///
/// ```
/// use pragma_sys::{SysError, ThreadPriority, set_current_thread_priority};
///
/// std::thread::spawn(|| {
///     if let Err(err) = set_current_thread_priority(ThreadPriority::Low) {
///         match err {
///             SysError::Unsupported { op, .. } => eprintln!("{op}: not on this platform"),
///             other => eprintln!("priority not changed: {other}"),
///         }
///     }
/// })
/// .join()
/// .map_err(|_| "thread panicked")?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SysError {
    /// The system call `op` returned OS error `code` (`GetLastError` / `errno`).
    #[error("{op} failed: OS error {code}")]
    #[non_exhaustive]
    Os {
        /// Which call.
        op: &'static str,
        /// OS error code.
        code: i32,
    },
    /// The OS denied permission (for example raising the priority on Linux without `CAP_SYS_NICE`).
    #[error("{op}: permission denied (OS error {code})")]
    #[non_exhaustive]
    PermissionDenied {
        /// Which call.
        op: &'static str,
        /// OS error code.
        code: i32,
    },
    /// The core number is outside the supported range `0..limit`.
    #[error("core {core} is out of range 0..{limit}")]
    #[non_exhaustive]
    InvalidCore {
        /// The requested core.
        core: usize,
        /// Upper bound (exclusive).
        limit: usize,
    },
    /// The thread name contains a NUL.
    #[error("thread name contains a NUL byte")]
    InvalidName,
    /// The operation is not supported on this platform.
    #[error("{op} is not supported on this platform")]
    #[non_exhaustive]
    Unsupported {
        /// Which operation.
        op: &'static str,
    },
}
