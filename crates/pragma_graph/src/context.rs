use std::any::type_name;
use std::error::Error;

use pragma_jobs::ThreadPool;

use crate::GraphError;
use crate::plan::Access;
use crate::resources::{Borrow, Res, ResMut, Resources};

/// Окружение задачи графа на время её исполнения.
///
/// Доступ к ресурсам проверяется по объявлению задачи: [`read`](Self::read) разрешён для
/// объявленных `reads`/`writes`, [`write`](Self::write) — только для `writes`. Через
/// [`pool`](Self::pool) задача может распараллелить свою работу (`parallel_for`, `join`, `scope`).
#[derive(Clone, Copy)]
pub struct TaskContext<'a> {
    pub(crate) name: &'a str,
    pub(crate) access: &'a [Access],
    pub(crate) pool: &'a ThreadPool,
    pub(crate) resources: &'a Resources,
}

impl std::fmt::Debug for TaskContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskContext")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl<'a> TaskContext<'a> {
    /// Имя задачи.
    #[must_use]
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// Пул, на котором исполняется граф: для вложенного параллелизма.
    #[must_use]
    pub fn pool(&self) -> &'a ThreadPool {
        self.pool
    }

    /// Ресурс на чтение (объявлен через `reads` или `writes`).
    pub fn read<T: 'static>(&self) -> Result<Res<'a, T>, GraphError> {
        self.check::<T>(false)?;
        self.resources
            .read::<T>()
            .map_err(|b| self.borrow_error::<T>(b))
    }

    /// Ресурс на запись (объявлен через `writes`).
    pub fn write<T: 'static>(&self) -> Result<ResMut<'a, T>, GraphError> {
        self.check::<T>(true)?;
        self.resources
            .write::<T>()
            .map_err(|b| self.borrow_error::<T>(b))
    }

    fn check<T: 'static>(&self, write: bool) -> Result<(), GraphError> {
        let id = std::any::TypeId::of::<T>();
        let declared = self
            .access
            .iter()
            .any(|a| a.key.id == id && (a.write || !write));
        if declared {
            Ok(())
        } else {
            Err(GraphError::UndeclaredAccess {
                task: self.name.to_owned(),
                resource: type_name::<T>().to_owned(),
                access: if write { "write" } else { "read" },
            })
        }
    }

    fn borrow_error<T: 'static>(&self, borrow: Borrow) -> GraphError {
        let task = self.name.to_owned();
        let resource = type_name::<T>().to_owned();
        match borrow {
            Borrow::Missing => GraphError::MissingResource { task, resource },
            Borrow::Busy => GraphError::BorrowConflict { task, resource },
        }
    }
}

/// Что может вернуть задача графа: `()` или `Result<(), E>`.
///
/// Ошибка `E` превращается в [`GraphError::TaskFailed`]; если это сама [`GraphError`]
/// (например, проброшенная через `?` из [`TaskContext::read`]), она возвращается как есть.
///
/// Трейт запечатан: реализовать его вне крейта нельзя (DECISIONS #42).
pub trait TaskResult: sealed::Sealed {
    /// Приводит результат задачи `task` к результату графа.
    fn into_result(self, task: &str) -> Result<(), GraphError>;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for () {}
    impl<E> Sealed for Result<(), E> {}
}

impl TaskResult for () {
    fn into_result(self, _task: &str) -> Result<(), GraphError> {
        Ok(())
    }
}

impl<E> TaskResult for Result<(), E>
where
    E: Into<Box<dyn Error + Send + Sync>>,
{
    fn into_result(self, task: &str) -> Result<(), GraphError> {
        self.map_err(|e| match e.into().downcast::<GraphError>() {
            Ok(graph) => *graph,
            Err(other) => GraphError::TaskFailed {
                task: task.to_owned(),
                message: other.to_string(),
            },
        })
    }
}
