use std::any::Any;

/// Ошибки жизненного цикла пула (создание, конфигурация).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JobsError {
    /// Некорректная конфигурация пула.
    #[error("invalid pool config: {reason}")]
    #[non_exhaustive]
    InvalidConfig {
        /// Что именно не так.
        reason: &'static str,
    },
    /// ОС отказала в создании рабочего потока.
    #[error("failed to spawn worker thread `{name}`")]
    #[non_exhaustive]
    SpawnFailed {
        /// Имя потока.
        name: String,
        /// Исходная ошибка ОС.
        #[source]
        source: std::io::Error,
    },
}

/// Ошибки исполнения задачи: возвращаются из `join`/`scope`/`parallel_*` и [`JobHandle::join`].
///
/// Отделены от [`JobsError`] (DECISIONS.md #13): исход задачи можно клонировать, сравнивать
/// и пересылать между потоками, а ошибка создания пула несёт `std::io::Error`.
///
/// [`JobHandle::join`]: crate::JobHandle::join
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum JobError {
    /// Задача запаниковала; паника перехвачена, воркер продолжает работу.
    #[error("job `{job}` panicked: {message}")]
    #[non_exhaustive]
    Panicked {
        /// Какая задача (например, `spawn`, `join b`, `parallel_for chunk`).
        job: String,
        /// Сообщение паники, если оно было строкой.
        message: String,
    },
    /// Задача сброшена без выполнения (пул остановлен с [`ShutdownPolicy::Cancel`]).
    ///
    /// [`ShutdownPolicy::Cancel`]: crate::ShutdownPolicy::Cancel
    #[error("job `{job}` was cancelled before it ran")]
    #[non_exhaustive]
    Cancelled {
        /// Какая задача.
        job: String,
    },
}

impl JobError {
    pub(crate) fn panicked(job: &str, payload: &(dyn Any + Send)) -> Self {
        let message = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            String::from("<non-string panic payload>")
        };
        Self::Panicked {
            job: job.to_owned(),
            message,
        }
    }

    pub(crate) fn cancelled(job: &str) -> Self {
        Self::Cancelled {
            job: job.to_owned(),
        }
    }
}
