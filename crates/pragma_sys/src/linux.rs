//! Linux: glibc calls via `libc`.

use std::ffi::CString;

use crate::{SysError, ThreadPriority};

/// `pthread_setname_np`: at most 16 bytes including the terminating nul.
const MAX_NAME: usize = 15;
/// Size of `cpu_set_t` in glibc.
const MAX_CORES: usize = libc::CPU_SETSIZE as usize;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn os_error(op: &'static str, code: i32) -> SysError {
    if code == libc::EPERM || code == libc::EACCES {
        SysError::PermissionDenied { op, code }
    } else {
        SysError::Os { op, code }
    }
}

pub(crate) fn set_current_thread_name(name: &str) -> Result<(), SysError> {
    let mut end = name.len().min(MAX_NAME);
    while !name.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let short = name.get(..end).unwrap_or_default();
    let cname = CString::new(short).map_err(|_| SysError::InvalidName)?;
    // SAFETY: `pthread_self` is always valid for the current thread; `cname` is a C string of at most
    // 16 bytes including the nul and lives until the end of the call.
    let code = unsafe { libc::pthread_setname_np(libc::pthread_self(), cname.as_ptr()) };
    if code != 0 {
        return Err(os_error("pthread_setname_np", code));
    }
    Ok(())
}

/// `nice` applies to a thread (tid), not to the whole process; that is how the Linux scheduler works.
fn tid() -> libc::id_t {
    // SAFETY: `gettid` has no preconditions.
    let tid = unsafe { libc::gettid() };
    libc::id_t::try_from(tid).unwrap_or(0)
}

pub(crate) fn set_current_thread_priority(priority: ThreadPriority) -> Result<(), SysError> {
    let nice = match priority {
        ThreadPriority::Low => 10,
        ThreadPriority::Normal => 0,
        ThreadPriority::High => -5,
    };
    // SAFETY: `setpriority` with `PRIO_PROCESS` and the current thread's tid changes only its `nice`.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid(), nice) } != 0 {
        return Err(os_error("setpriority", errno()));
    }
    Ok(())
}

pub(crate) fn current_thread_priority() -> Result<ThreadPriority, SysError> {
    // `getpriority` may legitimately return −1, so an error is told apart by errno, reset
    // before the call.
    // SAFETY: `__errno_location` returns a pointer to the current thread's errno.
    unsafe { *libc::__errno_location() = 0 };
    // SAFETY: see `set_current_thread_priority`; read-only.
    let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid()) };
    if nice == -1 {
        let code = errno();
        if code != 0 {
            return Err(os_error("getpriority", code));
        }
    }
    Ok(match nice {
        n if n > 0 => ThreadPriority::Low,
        n if n < 0 => ThreadPriority::High,
        _ => ThreadPriority::Normal,
    })
}

pub(crate) fn pin_current_thread(core: usize) -> Result<(), SysError> {
    if core >= MAX_CORES {
        return Err(SysError::InvalidCore {
            core,
            limit: MAX_CORES,
        });
    }
    // SAFETY: `cpu_set_t` is an array of integers; the all-zero bit pattern is a valid empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: `core < CPU_SETSIZE`, so the bit lies inside `set`.
    unsafe { libc::CPU_SET(core, &mut set) };
    // SAFETY: pid 0 is the current thread; the size and the pointer come from the local `set`.
    let code = unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    if code != 0 {
        return Err(os_error("sched_setaffinity", errno()));
    }
    Ok(())
}

pub(crate) fn current_core() -> Option<usize> {
    // SAFETY: no preconditions.
    let core = unsafe { libc::sched_getcpu() };
    usize::try_from(core).ok()
}
