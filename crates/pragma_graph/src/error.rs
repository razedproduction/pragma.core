use pragma_jobs::JobError;

/// Errors of building and running the graph.
///
/// Build errors ([`FrameGraphBuilder::build`]) are returned before the first frame and name the tasks;
/// frame errors ([`FrameGraph::run`]) are the first error of the task with the lowest registration
/// index, so with identical failures the result does not depend on thread scheduling.
///
/// [`FrameGraphBuilder::build`]: crate::FrameGraphBuilder::build
/// [`FrameGraph::run`]: crate::FrameGraph::run
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GraphError {
    /// The graph has a dependency cycle.
    #[error("dependency cycle: {}", tasks.join(" -> "))]
    #[non_exhaustive]
    Cycle {
        /// The tasks forming the cycle, in edge order (an edge leads from the last one to the first).
        tasks: Vec<String>,
    },
    /// Two tasks write (or write/read) the same resource without an explicit order.
    #[error("tasks `{a}` and `{b}` conflict on resource `{resource}` without explicit ordering")]
    #[non_exhaustive]
    AmbiguousOrder {
        /// The first task (in registration order).
        a: String,
        /// The second task.
        b: String,
        /// The conflicting resource.
        resource: String,
    },
    /// A dependency on a task that is not registered.
    #[error("task `{task}` depends on unknown task `{missing}`")]
    #[non_exhaustive]
    UnknownDependency {
        /// The task with the dependency.
        task: String,
        /// The missing task.
        missing: String,
    },
    /// A task name is registered twice.
    #[error("task `{task}` is registered twice")]
    #[non_exhaustive]
    DuplicateTask {
        /// The repeated name.
        task: String,
    },
    /// The graph does not fit the build limits ([`MAX_TASKS`](crate::MAX_TASKS),
    /// [`MAX_EDGES`](crate::MAX_EDGES)) or memory for the plan could not be allocated; returned
    /// instead of an `abort` on a large input.
    #[error("frame graph with {tasks} tasks is too large: {reason}")]
    #[non_exhaustive]
    TooLarge {
        /// Number of tasks in the description.
        tasks: usize,
        /// Which limit was exceeded (or out of memory).
        reason: &'static str,
    },
    /// A task declared a resource that is not in [`Resources`](crate::Resources).
    #[error("task `{task}` needs resource `{resource}`, which is missing")]
    #[non_exhaustive]
    MissingResource {
        /// The first task that declared the resource.
        task: String,
        /// The resource.
        resource: String,
    },
    /// A task accessed a resource without declaring that access.
    #[error("task `{task}` accessed `{resource}` for {access} without declaring it")]
    #[non_exhaustive]
    UndeclaredAccess {
        /// The task.
        task: String,
        /// The resource.
        resource: String,
        /// `"read"` or `"write"`.
        access: &'static str,
    },
    /// The resource is already borrowed (for example a task took it for writing twice).
    #[error("task `{task}`: resource `{resource}` is already borrowed")]
    #[non_exhaustive]
    BorrowConflict {
        /// The task.
        task: String,
        /// The resource.
        resource: String,
    },
    /// A task returned an error.
    #[error("task `{task}` failed: {message}")]
    #[non_exhaustive]
    TaskFailed {
        /// The task.
        task: String,
        /// The error text.
        message: String,
    },
    /// A task panicked; the panic was caught, and the dependent tasks of this frame did not run.
    #[error("task `{task}` panicked: {message}")]
    #[non_exhaustive]
    TaskPanicked {
        /// The task.
        task: String,
        /// The panic message, if it was a string.
        message: String,
    },
    /// A job pool error.
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
