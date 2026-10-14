use std::any::type_name;
use std::error::Error;

use pragma_jobs::ThreadPool;

use crate::GraphError;
use crate::plan::Access;
use crate::resources::{Borrow, Res, ResMut, Resources};

/// The environment of a graph task while it runs.
///
/// Resource access is checked against the task's declaration: [`read`](Self::read) is allowed for
/// declared `reads`/`writes`, [`write`](Self::write) only for `writes`. Through
/// [`pool`](Self::pool) a task can parallelize its own work (`parallel_for`, `join`, `scope`).
///
/// Access is checked at the call, not at build time: an undeclared access is a
/// [`GraphError::UndeclaredAccess`] for this task, never a data race.
///
/// ```
/// use pragma_graph::{FrameGraph, GraphError, Resources};
/// use pragma_jobs::{Grain, PoolConfig, ThreadPool};
///
/// struct Dt(f32);
/// struct Positions(Vec<f32>);
///
/// let mut g = FrameGraph::builder();
/// g.task("integrate", |t| t.reads::<Dt>().writes::<Positions>(), |ctx| -> Result<(), GraphError> {
///     let dt = ctx.read::<Dt>()?.0;
///     let mut positions = ctx.write::<Positions>()?;
///     ctx.pool().parallel_for(&mut positions.0, Grain::Auto, |x| *x += dt)?;
///     Ok(())
/// });
/// g.task("cheat", |t| t.reads::<Dt>(), |ctx| -> Result<(), GraphError> {
///     ctx.write::<Dt>()?.0 = 0.0; // declared only `reads`
///     Ok(())
/// });
/// let mut graph = g.build()?;
///
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
/// let mut res = Resources::new();
/// res.insert(Dt(0.5));
/// res.insert(Positions(vec![0.0; 1000]));
/// match graph.run(&pool, &mut res) {
///     Err(GraphError::UndeclaredAccess { task, access, .. }) => {
///         assert_eq!((task.as_str(), access), ("cheat", "write"));
///     }
///     other => panic!("unexpected {other:?}"),
/// }
/// // The independent task still ran.
/// assert_eq!(res.get::<Positions>().map(|p| p.0[0]), Some(0.5));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
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
    /// The task name.
    #[must_use]
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// The pool the graph runs on, for nested parallelism.
    #[must_use]
    pub fn pool(&self) -> &'a ThreadPool {
        self.pool
    }

    /// A resource for reading (declared via `reads` or `writes`).
    pub fn read<T: 'static>(&self) -> Result<Res<'a, T>, GraphError> {
        self.check::<T>(false)?;
        self.resources
            .read::<T>()
            .map_err(|b| self.borrow_error::<T>(b))
    }

    /// A resource for writing (declared via `writes`).
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

/// What a graph task may return: `()` or `Result<(), E>`.
///
/// An error `E` becomes [`GraphError::TaskFailed`]; if it is a [`GraphError`] itself
/// (for example propagated with `?` from [`TaskContext::read`]), it is returned as is.
///
/// The trait is sealed: it cannot be implemented outside the crate (DECISIONS #42).
///
/// ```
/// use pragma_graph::{FrameGraph, GraphError, Resources};
/// use pragma_jobs::{PoolConfig, ThreadPool};
///
/// #[derive(Debug)]
/// struct ScriptError(String);
/// impl std::fmt::Display for ScriptError {
///     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
///         write!(f, "script error: {}", self.0)
///     }
/// }
/// impl std::error::Error for ScriptError {}
///
/// let mut g = FrameGraph::builder();
/// g.task("scripts", |t| t, |_| Err(ScriptError("nil value".into())));
/// let mut graph = g.build()?;
///
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(1))?;
/// match graph.run(&pool, &mut Resources::new()) {
///     Err(GraphError::TaskFailed { task, message, .. }) => {
///         assert_eq!(task, "scripts");
///         assert_eq!(message, "script error: nil value");
///     }
///     other => panic!("unexpected {other:?}"),
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub trait TaskResult: sealed::Sealed {
    /// Converts the result of task `task` into a graph result.
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
