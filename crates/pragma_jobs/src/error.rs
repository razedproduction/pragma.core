/// Ошибки job system.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JobsError {
    /// Некорректная конфигурация пула.
    #[error("invalid pool config: {reason}")]
    InvalidConfig {
        /// Что именно не так.
        reason: &'static str,
    },
    /// ОС отказала в создании рабочего потока.
    #[error("failed to spawn worker thread `{name}`")]
    SpawnFailed {
        /// Имя потока.
        name: String,
        /// Исходная ошибка ОС.
        #[source]
        source: std::io::Error,
    },
}
