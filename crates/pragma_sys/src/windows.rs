//! Windows: Win32 API через `windows-sys`.

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessorNumber, GetCurrentThread, GetThreadPriority, SetThreadAffinityMask,
    SetThreadDescription, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL,
};

use crate::{SysError, ThreadPriority};

/// `GetThreadPriority` при ошибке (`MAXLONG`); в `windows-sys` константы нет.
const THREAD_PRIORITY_ERROR_RETURN: i32 = i32::MAX;

/// Наибольшая длина имени потока в единицах UTF-16 (без завершающего нуля).
const MAX_NAME: usize = 256;

/// `SetThreadAffinityMask` адресует ядра одной группы процессоров маской `usize`.
const MAX_CORES: usize = usize::BITS as usize;

fn last_error(op: &'static str) -> SysError {
    // SAFETY: `GetLastError` не имеет предусловий и читает значение потока.
    let code = unsafe { GetLastError() };
    SysError::Os {
        op,
        code: i32::try_from(code).unwrap_or(i32::MAX),
    }
}

pub(crate) fn set_current_thread_name(name: &str) -> Result<(), SysError> {
    // Копия под FFI не растёт со входом (reliability §1): длинное имя обрезается по символу.
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
    // SAFETY: `GetCurrentThread` возвращает псевдохэндл текущего потока (закрывать не нужно);
    // `wide` — строка UTF-16 с завершающим нулём, живёт до конца вызова и не изменяется.
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
    // SAFETY: псевдохэндл текущего потока имеет право `THREAD_SET_INFORMATION`; значение —
    // одна из документированных констант.
    if unsafe { SetThreadPriority(GetCurrentThread(), value) } == 0 {
        return Err(last_error("SetThreadPriority"));
    }
    Ok(())
}

pub(crate) fn current_thread_priority() -> Result<ThreadPriority, SysError> {
    // SAFETY: псевдохэндл текущего потока имеет право `THREAD_QUERY_INFORMATION`.
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
    // SAFETY: псевдохэндл текущего потока имеет право `THREAD_SET_INFORMATION`; маска с одним
    // битом. Ядро вне маски процесса — ошибка ОС (возврат 0), а не UB.
    if unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) } == 0 {
        return Err(last_error("SetThreadAffinityMask"));
    }
    Ok(())
}

pub(crate) fn current_core() -> Option<usize> {
    // SAFETY: без предусловий; номер ядра в группе текущего потока.
    let core = unsafe { GetCurrentProcessorNumber() };
    usize::try_from(core).ok()
}
