/// Ошибки платформенного слоя.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SysError {
    /// Системный вызов `op` вернул ошибку ОС `code` (`GetLastError` / `errno`).
    #[error("{op} failed: OS error {code}")]
    #[non_exhaustive]
    Os {
        /// Какой вызов.
        op: &'static str,
        /// Код ошибки ОС.
        code: i32,
    },
    /// ОС отказала в правах (например, повышение приоритета на Linux без `CAP_SYS_NICE`).
    #[error("{op}: permission denied (OS error {code})")]
    #[non_exhaustive]
    PermissionDenied {
        /// Какой вызов.
        op: &'static str,
        /// Код ошибки ОС.
        code: i32,
    },
    /// Номер ядра вне поддерживаемого диапазона `0..limit`.
    #[error("core {core} is out of range 0..{limit}")]
    #[non_exhaustive]
    InvalidCore {
        /// Запрошенное ядро.
        core: usize,
        /// Верхняя граница (не включительно).
        limit: usize,
    },
    /// Имя потока содержит NUL.
    #[error("thread name contains a NUL byte")]
    InvalidName,
    /// Операция не поддерживается на этой платформе.
    #[error("{op} is not supported on this platform")]
    #[non_exhaustive]
    Unsupported {
        /// Какая операция.
        op: &'static str,
    },
}
