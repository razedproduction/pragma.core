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
    #[non_exhaustive]
    Cycle {
        /// Задачи, образующие цикл, в порядке рёбер (от последней ребро ведёт к первой).
        tasks: Vec<String>,
    },
    /// Две задачи пишут (или пишут/читают) один ресурс без явного порядка.
    #[error("tasks `{a}` and `{b}` conflict on resource `{resource}` without explicit ordering")]
    #[non_exhaustive]
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
    #[non_exhaustive]
    UnknownDependency {
        /// Задача с зависимостью.
        task: String,
        /// Отсутствующая задача.
        missing: String,
    },
    /// Имя задачи зарегистрировано дважды.
    #[error("task `{task}` is registered twice")]
    #[non_exhaustive]
    DuplicateTask {
        /// Повторяющееся имя.
        task: String,
    },
    /// Граф не помещается в лимиты сборки ([`MAX_TASKS`](crate::MAX_TASKS),
    /// [`MAX_EDGES`](crate::MAX_EDGES)) или память для плана не выделилась — вместо `abort`
    /// на большом входе.
    #[error("frame graph with {tasks} tasks is too large: {reason}")]
    #[non_exhaustive]
    TooLarge {
        /// Число задач в описании.
        tasks: usize,
        /// Какой лимит превышен (или нехватка памяти).
        reason: &'static str,
    },
    /// Задача объявила ресурс, которого нет в [`Resources`](crate::Resources).
    #[error("task `{task}` needs resource `{resource}`, which is missing")]
    #[non_exhaustive]
    MissingResource {
        /// Первая задача, объявившая ресурс.
        task: String,
        /// Ресурс.
        resource: String,
    },
    /// Задача обратилась к ресурсу, не объявив такой доступ.
    #[error("task `{task}` accessed `{resource}` for {access} without declaring it")]
    #[non_exhaustive]
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
    #[non_exhaustive]
    BorrowConflict {
        /// Задача.
        task: String,
        /// Ресурс.
        resource: String,
    },
    /// Задача вернула ошибку.
    #[error("task `{task}` failed: {message}")]
    #[non_exhaustive]
    TaskFailed {
        /// Задача.
        task: String,
        /// Текст ошибки.
        message: String,
    },
    /// Задача запаниковала; паника перехвачена, зависимые задачи этого кадра не запускались.
    #[error("task `{task}` panicked: {message}")]
    #[non_exhaustive]
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
