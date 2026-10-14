//! Windows: the Win32 API via `windows-sys`.

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessorNumber, GetCurrentThread, GetThreadPriority, SetThreadAffinityMask,
    SetThreadDescription, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL,
};

use crate::{SysError, ThreadPriority};

/// What `GetThreadPriority` returns on error (`MAXLONG`); `windows-sys` has no constant for it.
const THREAD_PRIORITY_ERROR_RETURN: i32 = i32::MAX;

/// Maximum thread name length in UTF-16 units (without the terminating nul).
const MAX_NAME: usize = 256;

/// `SetThreadAffinityMask` addresses the cores of one processor group with a `usize` mask.
const MAX_CORES: usize = usize::BITS as usize;

fn last_error(op: &'static str) -> SysError {
    // SAFETY: `GetLastError` has no preconditions and reads a per-thread value.
    let code = unsafe { GetLastError() };
    SysError::Os {
        op,
        code: i32::try_from(code).unwrap_or(i32::MAX),
    }
}

pub(crate) fn set_current_thread_name(name: &str) -> Result<(), SysError> {
    // The FFI copy does not grow with the input (reliability §1): a long name is cut at a character.
    let mut wide: Vec<u16> = Vec::with_capacity(MAX_NAME.saturating_add(1));
    for c in name.chars() {
        let mut buf = [0_u16; 2];
        let units = c.encode_utf16(&mut buf);
        if wide.len().saturating_add(units.len()) > MAX_NAME {
            break;
        }
        wide.extend_from_slice(units);
    }
    wide.push(0);
    // SAFETY: `GetCurrentThread` returns a pseudo-handle of the current thread (no need to close it);
    // `wide` is a nul-terminated UTF-16 string that lives until the end of the call and is not modified.
    let hr = unsafe { SetThreadDescription(GetCurrentThread(), wide.as_ptr()) };
    if hr < 0 {
        return Err(SysError::Os {
            op: "SetThreadDescription",
            code: hr,
        });
    }
    Ok(())
}

pub(crate) fn set_current_thread_priority(priority: ThreadPriority) -> Result<(), SysError> {
    let value = match priority {
        ThreadPriority::Low => THREAD_PRIORITY_BELOW_NORMAL,
        ThreadPriority::Normal => THREAD_PRIORITY_NORMAL,
        ThreadPriority::High => THREAD_PRIORITY_ABOVE_NORMAL,
    };
    // SAFETY: the current-thread pseudo-handle has `THREAD_SET_INFORMATION` access; the value is
    // one of the documented constants.
    if unsafe { SetThreadPriority(GetCurrentThread(), value) } == 0 {
        return Err(last_error("SetThreadPriority"));
    }
    Ok(())
}

pub(crate) fn current_thread_priority() -> Result<ThreadPriority, SysError> {
    // SAFETY: the current-thread pseudo-handle has `THREAD_QUERY_INFORMATION` access.
    let value = unsafe { GetThreadPriority(GetCurrentThread()) };
    if value == THREAD_PRIORITY_ERROR_RETURN {
        return Err(last_error("GetThreadPriority"));
    }
    Ok(match value {
        v if v < THREAD_PRIORITY_NORMAL => ThreadPriority::Low,
        v if v > THREAD_PRIORITY_NORMAL => ThreadPriority::High,
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
    let mask = 1_usize.checked_shl(u32::try_from(core).unwrap_or(u32::MAX));
    let Some(mask) = mask else {
        return Err(SysError::InvalidCore {
            core,
            limit: MAX_CORES,
        });
    };
    // SAFETY: the current-thread pseudo-handle has `THREAD_SET_INFORMATION` access; the mask has a
    // single bit. A core outside the process mask is an OS error (return value 0), not UB.
    if unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) } == 0 {
        return Err(last_error("SetThreadAffinityMask"));
    }
    Ok(())
}

pub(crate) fn current_core() -> Option<usize> {
    // SAFETY: no preconditions; the core number within the current thread's group.
    let core = unsafe { GetCurrentProcessorNumber() };
    usize::try_from(core).ok()
}
