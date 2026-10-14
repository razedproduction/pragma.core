//! Платформы вне DECISIONS #2: операции не поддерживаются, но и не паникуют.

use crate::{SysError, ThreadPriority};

pub(crate) fn set_current_thread_name(_name: &str) -> Result<(), SysError> {
    Err(SysError::Unsupported {
        op: "set_current_thread_name",
    })
}

pub(crate) fn set_current_thread_priority(_priority: ThreadPriority) -> Result<(), SysError> {
    Err(SysError::Unsupported {
        op: "set_current_thread_priority",
    })
}

pub(crate) fn current_thread_priority() -> Result<ThreadPriority, SysError> {
    Err(SysError::Unsupported {
        op: "current_thread_priority",
    })
}

pub(crate) fn pin_current_thread(_core: usize) -> Result<(), SysError> {
    Err(SysError::Unsupported {
        op: "pin_current_thread",
    })
}

pub(crate) fn current_core() -> Option<usize> {
    None
}
