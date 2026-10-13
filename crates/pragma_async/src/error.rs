use std::any::Any;

/// Ошибки async-задач и исполнителя.
///
/// `Clone`/`PartialEq`, как и `JobError`: исход задачи пересылают между потоками и сравнивают.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AsyncError {
    /// Задача отменена до завершения: сброшен её хэндл, пул или исполнитель остановлен.
    #[error("task was cancelled")]
    Cancelled,
    /// Задача запаниковала; паника перехвачена и не уронила воркер.
    #[error("task `{task}` panicked: {message}")]
    Panicked {
        /// Какая задача: `spawn_async` или `unblock`.
        task: &'static str,
        /// Сообщение паники, если оно было строкой.
        message: String,
    },
    /// Blocking-пул переполнен: задача не принята, вызывающий не ждал.
    #[error("blocking pool is full ({capacity} pending tasks)")]
    Overloaded {
        /// Лимит одновременно поставленных blocking-задач.
        capacity: usize,
    },
    /// ОС не дала создать поток blocking-пула, а свободных потоков нет.
    #[error("failed to spawn blocking thread `{thread}`: {message}")]
    SpawnFailed {
        /// Имя потока.
        thread: String,
        /// Текст ошибки ОС (`std::io::Error` не `Clone`).
        message: String,
    },
    /// Блокирующее ожидание вызвано из работы пула (воркер или кадр графа): оно задержало бы
    /// кадр или повесило пул. Используйте `poll_ready` или `.await`.
    #[error("blocking wait inside pool work is not allowed; use poll_ready() or .await")]
    WouldBlock,
    /// Результат уже забран из хэндла.
    #[error("task result was already taken")]
    Consumed,
    /// Некорректная конфигурация исполнителя.
    #[error("invalid async config: {reason}")]
    InvalidConfig {
        /// Что именно не так.
        reason: &'static str,
    },
}

impl AsyncError {
    pub(crate) fn panicked(task: &'static str, payload: &(dyn Any + Send)) -> Self {
        let message = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            String::from("<non-string panic payload>")
        };
        Self::Panicked { task, message }
    }
}
