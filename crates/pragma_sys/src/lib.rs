//! L1: платформенный слой `pragma.core` — имена, приоритеты и привязка потоков к ядрам.
//!
//! Второй (после `pragma_sync`) крейт, где разрешён `unsafe` (`layers.toml`, DECISIONS #27):
//! здесь живут только вызовы системных FFI (`windows-sys` на Windows, `libc` на Linux), каждый
//! с комментарием `// SAFETY:`. Остальные крейты пользуются безопасным API отсюда, в
//! `pragma_jobs` он доступен как `pragma_jobs::sys`.
//!
//! Все функции действуют на **текущий** поток. На других платформах они возвращают
//! [`SysError::Unsupported`], а не паникуют: платформенные настройки — оптимизация, без
//! которой ядро работает.
//!
//! ```
//! use pragma_sys::{ThreadPriority, set_current_thread_name, set_current_thread_priority};
//!
//! std::thread::spawn(|| {
//!     set_current_thread_name("asset-loader")?;
//!     // Понизить приоритет можно без прав администратора; повышение может вернуть
//!     // `SysError::PermissionDenied` (Linux без CAP_SYS_NICE).
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

/// Приоритет потока относительно остальных потоков процесса.
///
/// | | Windows | Linux (nice) |
/// |---|---|---|
/// | `Low` | `THREAD_PRIORITY_BELOW_NORMAL` | 10 |
/// | `Normal` | `THREAD_PRIORITY_NORMAL` | 0 |
/// | `High` | `THREAD_PRIORITY_ABOVE_NORMAL` | −5 (нужен `CAP_SYS_NICE`) |
///
/// На Linux без прав нельзя и вернуть `nice` назад к меньшему значению: после `Low` вызов
/// `Normal` вернёт [`SysError::PermissionDenied`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ThreadPriority {
    /// Ниже обычного: фоновая работа, которая не должна мешать кадру.
    Low,
    /// Обычный приоритет ОС.
    #[default]
    Normal,
    /// Выше обычного: потоки кадра.
    High,
}

/// Задаёт имя текущего потока, видимое в отладчике и профайлере.
///
/// Потоки пула `pragma_jobs` уже названы через `std::thread::Builder`; функция нужна для
/// потоков, созданных не через `Builder` (например, главного). Имя обрезается по границе
/// символа: на Linux до 15 байт (ограничение ядра), на Windows до 256 единиц UTF-16.
pub fn set_current_thread_name(name: &str) -> Result<(), SysError> {
    if name.contains('\0') {
        return Err(SysError::InvalidName);
    }
    imp::set_current_thread_name(name)
}

/// Задаёт приоритет текущего потока.
pub fn set_current_thread_priority(priority: ThreadPriority) -> Result<(), SysError> {
    imp::set_current_thread_priority(priority)
}

/// Приоритет текущего потока, приведённый к [`ThreadPriority`] (любое значение ниже
/// обычного — `Low`, выше — `High`).
pub fn current_thread_priority() -> Result<ThreadPriority, SysError> {
    imp::current_thread_priority()
}

/// Привязывает текущий поток к логическому ядру `core` (нумерация с нуля).
///
/// На Windows поддерживаются ядра первой группы процессоров (`core < 64`), на Linux —
/// `core < 1024` (`CPU_SETSIZE`); ядро вне диапазона — [`SysError::InvalidCore`].
pub fn pin_current_thread(core: usize) -> Result<(), SysError> {
    imp::pin_current_thread(core)
}

/// Логическое ядро, на котором поток исполняется сейчас (`None`, если ОС не сообщает).
///
/// Без привязки значение может измениться сразу после вызова; годится для диагностики и
/// для проверки [`pin_current_thread`].
#[must_use]
pub fn current_core() -> Option<usize> {
    imp::current_core()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Каждая проверка — в своём потоке: приоритет и привязка меняются у текущего потока.
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
            // Перенос на нужное ядро — при следующем планировании.
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
