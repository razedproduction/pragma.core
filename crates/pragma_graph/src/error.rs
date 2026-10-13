use pragma_jobs::JobError;

/// Ошибки построения и исполнения графа.
///
/// Ошибки сборки ([`FrameGraphBuilder::build`]) возвращаются до первого кадра и называют задачи
/// по именам; ошибки кадра ([`FrameGraph::run`]) — первая ошибка задачи с наименьшим индексом
/// регистрации, поэтому при одинаковых сбоях результат не зависит от расписания потоков.
///
/// [`FrameGraphBuilder::build`]: crate::FrameGraphBuilder::build
/// [`FrameGraph::run`]: crate::FrameGraph::run
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GraphError {
    /// В графе есть цикл зависимостей.
    #[error("dependency cycle: {}", tasks.join(" -> "))]
    Cycle {
        /// Задачи, образующие цикл, в порядке рёбер (от последней ребро ведёт к первой).
        tasks: Vec<String>,
    },
    /// Две задачи пишут (или пишут/читают) один ресурс без явного порядка.
    #[error("tasks `{a}` and `{b}` conflict on resource `{resource}` without explicit ordering")]
    AmbiguousOrder {
        /// Первая задача (в порядке регистрации).
        a: String,
        /// Вторая задача.
        b: String,
        /// Ресурс конфликта.
        resource: String,
    },
    /// Зависимость на незарегистрированную задачу.
    #[error("task `{task}` depends on unknown task `{missing}`")]
    UnknownDependency {
        /// Задача с зависимостью.
        task: String,
        /// Отсутствующая задача.
        missing: String,
    },
    /// Имя задачи зарегистрировано дважды.
    #[error("task `{task}` is registered twice")]
    DuplicateTask {
        /// Повторяющееся имя.
        task: String,
    },
    /// Задача объявила ресурс, которого нет в [`Resources`](crate::Resources).
    #[error("task `{task}` needs resource `{resource}`, which is missing")]
    MissingResource {
        /// Первая задача, объявившая ресурс.
        task: String,
        /// Ресурс.
        resource: String,
    },
    /// Задача обратилась к ресурсу, не объявив такой доступ.
    #[error("task `{task}` accessed `{resource}` for {access} without declaring it")]
    UndeclaredAccess {
        /// Задача.
        task: String,
        /// Ресурс.
        resource: String,
        /// `"read"` или `"write"`.
        access: &'static str,
    },
    /// Ресурс уже заимствован (например, задача дважды взяла его на запись).
    #[error("task `{task}`: resource `{resource}` is already borrowed")]
    BorrowConflict {
        /// Задача.
        task: String,
        /// Ресурс.
        resource: String,
    },
    /// Задача вернула ошибку.
    #[error("task `{task}` failed: {message}")]
    TaskFailed {
        /// Задача.
        task: String,
        /// Текст ошибки.
        message: String,
    },
    /// Задача запаниковала; паника перехвачена, зависимые задачи этого кадра не запускались.
    #[error("task `{task}` panicked: {message}")]
    TaskPanicked {
        /// Задача.
        task: String,
        /// Сообщение паники, если оно было строкой.
        message: String,
    },
    /// Ошибка пула задач.
    #[error(transparent)]
    Job(#[from] JobError),
}

impl GraphError {
    pub(crate) fn panicked(task: &str, payload: &(dyn std::any::Any + Send)) -> Self {
        let message = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            String::from("<non-string panic payload>")
        };
        Self::TaskPanicked {
            task: task.to_owned(),
            message,
        }
    }
}
