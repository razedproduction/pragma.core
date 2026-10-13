//! L3: граф задач кадра.
//!
//! Статус: каркас (веха M2 в `docs/roadmap.md`). Задачи объявляют доступ к
//! ресурсам (чтение/запись); граф строит DAG, проверяет циклы и неявные
//! конфликты при сборке, а не во время кадра, и исполняет непересекающиеся
//! задачи параллельно через `pragma_jobs`.
//!
//! ```
//! use pragma_graph::GraphError;
//!
//! let err = GraphError::Cycle { tasks: vec!["physics".into(), "transforms".into()] };
//! assert!(err.to_string().contains("physics"));
//! ```
#![forbid(unsafe_code)]

/// Ошибки построения и исполнения графа.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GraphError {
    /// В графе есть цикл зависимостей.
    #[error("dependency cycle: {}", tasks.join(" -> "))]
    Cycle {
        /// Задачи, образующие цикл.
        tasks: Vec<String>,
    },
    /// Две задачи пишут (или пишут/читают) один ресурс без явного порядка.
    #[error("tasks `{a}` and `{b}` conflict on resource `{resource}` without explicit ordering")]
    AmbiguousOrder {
        /// Первая задача.
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
}
