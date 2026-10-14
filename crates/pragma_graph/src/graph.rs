use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use pragma_jobs::{Priority, ThreadPool};
use pragma_sync::__private::ArrivalCounter;

use crate::GraphError;
use crate::context::{TaskContext, TaskResult};
use crate::plan::{self, Access, Dependency, TaskSpec};
use crate::resources::{ResourceKey, Resources};

type System = Box<dyn Fn(&TaskContext<'_>) -> Result<(), GraphError> + Send + Sync>;

/// A task declaration: resource access and explicit order.
///
/// Passed to the configuration closure of [`FrameGraphBuilder::task`]:
/// `|t| t.reads::<Input>().writes::<Bodies>().after("input")`.
#[derive(Debug, Default)]
#[must_use]
pub struct TaskConfig {
    access: Vec<Access>,
    deps: Vec<Dependency>,
}

impl TaskConfig {
    /// The task reads resource `T`.
    pub fn reads<T: Send + Sync + 'static>(self) -> Self {
        self.declare(ResourceKey::of::<T>(), false)
    }

    /// The task writes resource `T` (writing includes reading).
    pub fn writes<T: Send + Sync + 'static>(self) -> Self {
        self.declare(ResourceKey::of::<T>(), true)
    }

    /// The task runs before task `task`.
    pub fn before(mut self, task: impl Into<String>) -> Self {
        self.deps.push(Dependency::Before(task.into()));
        self
    }

    /// The task runs after task `task`.
    pub fn after(mut self, task: impl Into<String>) -> Self {
        self.deps.push(Dependency::After(task.into()));
        self
    }

    fn declare(mut self, key: ResourceKey, write: bool) -> Self {
        match self.access.iter_mut().find(|a| a.key.id == key.id) {
            Some(existing) => existing.write |= write,
            None => self.access.push(Access { key, write }),
        }
        self
    }
}

struct TaskDecl {
    name: String,
    config: TaskConfig,
    system: System,
}

/// The frame graph builder (see [`FrameGraph::builder`]).
#[derive(Default)]
#[must_use]
pub struct FrameGraphBuilder {
    tasks: Vec<TaskDecl>,
    ordered: bool,
}

impl std::fmt::Debug for FrameGraphBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameGraphBuilder")
            .field(
                "tasks",
                &self.tasks.iter().map(|t| &t.name).collect::<Vec<_>>(),
            )
            .field("ordered", &self.ordered)
            .finish()
    }
}

impl FrameGraphBuilder {
    /// The `ordered` mode: conflicting tasks without an explicit order run in declaration
    /// order instead of producing [`GraphError::AmbiguousOrder`]. Explicit `before`/`after` take
    /// precedence over declaration order; if together they close a cycle, that is [`GraphError::Cycle`].
    pub fn ordered(mut self) -> Self {
        self.ordered = true;
        self
    }

    /// Registers task `name`: `configure` declares access and order, `system` is the work.
    ///
    /// `system` is called once per frame; it returns `()` or `Result<(), E>` (see [`TaskResult`]).
    pub fn task<C, F, R>(&mut self, name: impl Into<String>, configure: C, system: F) -> &mut Self
    where
        C: FnOnce(TaskConfig) -> TaskConfig,
        F: Fn(&TaskContext<'_>) -> R + Send + Sync + 'static,
        R: TaskResult,
    {
        let name = name.into();
        let config = configure(TaskConfig::default());
        let system: System = Box::new(move |ctx| system(ctx).into_result(ctx.name()));
        self.tasks.push(TaskDecl {
            name,
            config,
            system,
        });
        self
    }

    /// Validates the description and builds the graph: duplicate names ([`GraphError::DuplicateTask`]),
    /// unknown dependencies ([`GraphError::UnknownDependency`]), cycles
    /// ([`GraphError::Cycle`]) and unordered access conflicts ([`GraphError::AmbiguousOrder`]).
    /// A graph with more than [`MAX_TASKS`](crate::MAX_TASKS) tasks or [`MAX_EDGES`](crate::MAX_EDGES)
    /// edges, as well as running out of memory while planning, is [`GraphError::TooLarge`].
    pub fn build(self) -> Result<FrameGraph, GraphError> {
        // Before any allocation sized by the number of tasks (reliability §1).
        if self.tasks.len() > plan::MAX_TASKS {
            return Err(GraphError::TooLarge {
                tasks: self.tasks.len(),
                reason: plan::TOO_MANY_TASKS,
            });
        }
        let specs: Vec<TaskSpec<'_>> = self
            .tasks
            .iter()
            .map(|t| TaskSpec {
                name: &t.name,
                access: &t.config.access,
                deps: &t.config.deps,
            })
            .collect();
        let plan = plan::plan(&specs, self.ordered)?;
        drop(specs);

        let mut indegree = vec![0_usize; self.tasks.len()];
        for &v in plan.successors.iter().flatten() {
            if let Some(d) = indegree.get_mut(v) {
                *d = d.saturating_add(1);
            }
        }
        let mut required: Vec<(ResourceKey, usize)> = Vec::new();
        for (i, task) in self.tasks.iter().enumerate() {
            for access in &task.config.access {
                if !required.iter().any(|(k, _)| k.id == access.key.id) {
                    required.push((access.key, i));
                }
            }
        }
        let nodes: Vec<Node> = self
            .tasks
            .into_iter()
            .zip(plan.successors)
            .zip(&indegree)
            .map(|((task, successors), &indegree)| Node {
                ready: successors.iter().map(|_| AtomicUsize::new(0)).collect(),
                name: task.name,
                access: task.config.access,
                system: task.system,
                successors,
                indegree,
            })
            .collect();
        let roots = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.indegree == 0)
            .map(|(i, _)| AtomicUsize::new(i))
            .collect();
        let remaining = nodes.iter().map(|_| ArrivalCounter::new(0)).collect();
        Ok(FrameGraph {
            nodes,
            order: plan.order,
            roots,
            remaining,
            required,
        })
    }
}

pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) access: Vec<Access>,
    system: System,
    /// Direct successors (after transitive reduction), in ascending index order.
    pub(crate) successors: Vec<usize>,
    /// Per-frame scratch: successors that became ready after this task. Written only by
    /// the thread that ran the task (each task runs once per frame).
    ready: Box<[AtomicUsize]>,
    indegree: usize,
}

/// The frame graph: built once ([`FrameGraph::builder`]), run every frame
/// ([`FrameGraph::run`]) without allocations.
pub struct FrameGraph {
    pub(crate) nodes: Vec<Node>,
    order: Vec<usize>,
    /// Tasks without predecessors (indices; atomics so the code can be shared with `Node::ready`).
    roots: Box<[AtomicUsize]>,
    /// How many of a task's predecessors have not finished yet in the current frame.
    remaining: Box<[ArrivalCounter]>,
    /// All resources of the graph and the first task that declared each one.
    required: Vec<(ResourceKey, usize)>,
}

impl std::fmt::Debug for FrameGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameGraph")
            .field("order", &self.order().collect::<Vec<_>>())
            .field("edges", &self.edges().count())
            .finish()
    }
}

impl FrameGraph {
    /// A new builder.
    pub fn builder() -> FrameGraphBuilder {
        FrameGraphBuilder::default()
    }

    /// Number of tasks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the graph has no tasks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Task names in a stable topological order: among ready tasks, the one registered earlier
    /// comes first. On a single worker the tasks do not necessarily run in this
    /// order, but any actual order is consistent with the same edges.
    pub fn order(&self) -> impl Iterator<Item = &str> {
        self.order
            .iter()
            .filter_map(|&i| self.nodes.get(i).map(|n| n.name.as_str()))
    }

    /// "Earlier → later" edges after transitive reduction, in topological order of their sources.
    pub fn edges(&self) -> impl Iterator<Item = (&str, &str)> {
        self.order
            .iter()
            .filter_map(|&i| self.nodes.get(i))
            .flat_map(move |node| {
                node.successors.iter().filter_map(move |&s| {
                    self.nodes
                        .get(s)
                        .map(|succ| (node.name.as_str(), succ.name.as_str()))
                })
            })
    }

    /// Runs a frame: every task exactly once, after all of its predecessors;
    /// independent tasks in parallel on `pool`.
    ///
    /// Before starting, it checks that `resources` contains all declared resources
    /// ([`GraphError::MissingResource`]). An error or panic of a task does not stop independent
    /// branches, but the tasks depending on it do not run in this frame; the error of the task
    /// with the lowest registration index is returned. In the steady state it does not allocate.
    pub fn run(&mut self, pool: &ThreadPool, resources: &mut Resources) -> Result<(), GraphError> {
        let _span = tracing::debug_span!("frame_graph", tasks = self.nodes.len()).entered();
        #[cfg(feature = "puffin")]
        puffin::profile_scope!("frame_graph");
        for &(key, task) in &self.required {
            if !resources.contains_key(key) {
                return Err(GraphError::MissingResource {
                    task: self
                        .nodes
                        .get(task)
                        .map(|n| n.name.clone())
                        .unwrap_or_default(),
                    resource: key.name.to_owned(),
                });
            }
        }
        for (counter, node) in self.remaining.iter_mut().zip(&self.nodes) {
            counter.reset(node.indegree);
        }
        let first_error = Mutex::new(None);
        let run = Run {
            nodes: &self.nodes,
            remaining: &self.remaining,
            pool,
            resources,
            first_error: &first_error,
        };
        let roots = &self.roots;
        // One scope per frame: the external thread enters the pool once rather than on every `join`.
        if let Err(e) = pool.scope(|_| run.fork(roots)) {
            run.record(usize::MAX, e.into());
        }
        match first_error
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
        {
            Some((_, error)) => Err(error),
            None => Ok(()),
        }
    }
}

/// The state of one frame.
struct Run<'a> {
    nodes: &'a [Node],
    remaining: &'a [ArrivalCounter],
    pool: &'a ThreadPool,
    resources: &'a Resources,
    /// The error of the task with the lowest index (`usize::MAX`: an error outside tasks).
    first_error: &'a Mutex<Option<(usize, GraphError)>>,
}

impl Run<'_> {
    fn record(&self, index: usize, error: GraphError) {
        let mut slot = self
            .first_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if slot.as_ref().is_none_or(|(prev, _)| index < *prev) {
            *slot = Some((index, error));
        }
    }

    /// Launches ready tasks: half of them here, the other half handed to thieves via `join`
    /// (`join` does not allocate, unlike `spawn`).
    fn fork(&self, ready: &[AtomicUsize]) {
        match ready {
            [] => {}
            [one] => self.run_from(one.load(Ordering::Relaxed)),
            _ => {
                let (left, right) = ready.split_at(ready.len() / 2);
                if let Err(e) = self.pool.join(|| self.fork(left), || self.fork(right)) {
                    self.record(usize::MAX, e.into());
                }
            }
        }
    }

    /// Runs a task and releases its successors. A chain of single ready successors
    /// is continued in a loop, without growing the stack.
    fn run_from(&self, mut current: usize) {
        loop {
            let Some(node) = self.nodes.get(current) else {
                return;
            };
            if !self.execute(current, node) {
                return;
            }
            let mut ready = 0_usize;
            for &succ in &node.successors {
                let Some(counter) = self.remaining.get(succ) else {
                    continue;
                };
                // The last predecessor to arrive sees the writes of all the others and publishes them,
                // together with its own, to the task it launches.
                if counter.arrive()
                    && let Some(slot) = node.ready.get(ready)
                {
                    slot.store(succ, Ordering::Relaxed);
                    ready = ready.saturating_add(1);
                }
            }
            match node.ready.get(..ready) {
                Some([one]) => current = one.load(Ordering::Relaxed),
                Some(many) if !many.is_empty() => return self.fork(many),
                _ => return,
            }
        }
    }

    fn execute(&self, index: usize, node: &Node) -> bool {
        let ctx = TaskContext {
            name: &node.name,
            access: &node.access,
            pool: self.pool,
            resources: self.resources,
        };
        // The span fields are evaluated only if the DEBUG level is enabled.
        let _span = tracing::debug_span!(
            "graph_task",
            task = node.name.as_str(),
            priority = pragma_jobs::current_priority().map(Priority::as_str),
            worker = self.pool.current_worker_index(),
        )
        .entered();
        #[cfg(feature = "puffin")]
        puffin::profile_scope!("graph_task", node.name.as_str());
        let system = || pragma_jobs::labeled("graph", &node.name, || (node.system)(&ctx));
        match panic::catch_unwind(AssertUnwindSafe(system)) {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                self.record(index, error);
                false
            }
            Err(payload) => {
                self.record(index, GraphError::panicked(&node.name, &*payload));
                false
            }
        }
    }
}
