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
///
/// Declaring the same type twice merges the access (a `reads` and a `writes` is a write).
/// `before`/`after` add ordering on top of the data dependencies; they are also how two tasks that
/// share no resource are ordered (for example "audio after animation, for latency").
///
/// ```
/// use pragma_graph::FrameGraph;
///
/// struct Transforms;
/// struct Skins;
///
/// let mut g = FrameGraph::builder();
/// g.task("animate", |t| t.writes::<Transforms>(), |_| ());
/// g.task("skin", |t| t.reads::<Transforms>().writes::<Skins>().after("animate"), |_| ());
/// g.task("audio", |t| t.after("animate").before("skin"), |_| ());
/// let graph = g.build()?;
/// assert_eq!(graph.order().collect::<Vec<_>>(), ["animate", "audio", "skin"]);
/// # Ok::<(), pragma_graph::GraphError>(())
/// ```
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
///
/// `task` takes `&mut self`, so tasks can be registered in a loop or by several engine plugins in
/// turn; [`build`](Self::build) consumes the builder.
///
/// ```
/// use pragma_graph::{FrameGraph, FrameGraphBuilder};
///
/// struct World(u32);
///
/// fn register_gameplay(g: &mut FrameGraphBuilder) {
///     g.task("spawn", |t| t.writes::<World>(), |ctx| {
///         ctx.write::<World>().map(|mut w| w.0 += 1)
///     });
/// }
///
/// let mut g = FrameGraph::builder();
/// register_gameplay(&mut g);
/// let graph = g.build()?;
/// assert_eq!(graph.len(), 1);
/// # Ok::<(), pragma_graph::GraphError>(())
/// ```
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
    ///
    /// Convenient when porting a sequential loop of systems: the registration order becomes the
    /// order for every conflict, and independent systems still run in parallel.
    ///
    /// ```
    /// use pragma_graph::FrameGraph;
    ///
    /// struct Score(u32);
    ///
    /// let mut g = FrameGraph::builder().ordered();
    /// g.task("add", |t| t.writes::<Score>(), |_| ());
    /// g.task("double", |t| t.writes::<Score>(), |_| ()); // no `after` needed
    /// let graph = g.build()?;
    /// assert_eq!(graph.edges().collect::<Vec<_>>(), [("add", "double")]);
    /// # Ok::<(), pragma_graph::GraphError>(())
    /// ```
    pub fn ordered(mut self) -> Self {
        self.ordered = true;
        self
    }

    /// Registers task `name`: `configure` declares access and order, `system` is the work.
    ///
    /// `system` is called once per frame; it returns `()` or `Result<(), E>` (see [`TaskResult`]).
    /// It is `Fn + Send + Sync + 'static`: per-frame data goes through [`Resources`], not through
    /// captured mutable state.
    ///
    /// [`Resources`]: crate::Resources
    ///
    /// ```
    /// use pragma_graph::{FrameGraph, GraphError, Resources};
    ///
    /// struct Gravity(f32);
    /// struct Velocities(Vec<f32>);
    ///
    /// let mut g = FrameGraph::builder();
    /// g.task(
    ///     "gravity",
    ///     |t| t.reads::<Gravity>().writes::<Velocities>(),
    ///     |ctx| -> Result<(), GraphError> {
    ///         let g = ctx.read::<Gravity>()?.0;
    ///         ctx.write::<Velocities>()?.0.iter_mut().for_each(|v| *v -= g);
    ///         Ok(())
    ///     },
    /// );
    /// let mut graph = g.build()?;
    ///
    /// let pool = pragma_jobs::ThreadPool::new(pragma_jobs::PoolConfig::default().with_workers(1))?;
    /// let mut res = Resources::new();
    /// res.insert(Gravity(9.8));
    /// res.insert(Velocities(vec![0.0; 3]));
    /// graph.run(&pool, &mut res)?;
    /// assert_eq!(res.get::<Velocities>().map(|v| v.0[0]), Some(-9.8));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
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
    ///
    /// Build once at startup (or when the set of systems changes), not every frame.
    ///
    /// ```
    /// use pragma_graph::{FrameGraph, GraphError};
    ///
    /// let mut g = FrameGraph::builder();
    /// g.task("a", |t| t.after("b"), |_| ());
    /// g.task("b", |t| t.after("a"), |_| ());
    /// match g.build() {
    ///     Err(GraphError::Cycle { tasks, .. }) => assert_eq!(tasks.len(), 2),
    ///     other => panic!("expected a cycle, got {other:?}"),
    /// }
    ///
    /// let mut g = FrameGraph::builder();
    /// g.task("render", |t| t.after("physics"), |_| ());
    /// assert!(matches!(g.build(), Err(GraphError::UnknownDependency { .. })));
    /// ```
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
///
/// A typical fixed-step loop: the graph and the pool are created once, the resources carry the
/// state from tick to tick.
///
/// ```
/// use pragma_base::Tick;
/// use pragma_graph::{FrameGraph, Resources};
/// use pragma_jobs::{PoolConfig, ThreadPool};
///
/// type BoxError = Box<dyn std::error::Error + Send + Sync>;
///
/// struct Clock(Tick);
/// struct Positions(Vec<f32>);
///
/// let mut g = FrameGraph::builder();
/// g.task("clock", |t| t.writes::<Clock>(), |ctx| -> Result<(), BoxError> {
///     let mut clock = ctx.write::<Clock>()?;
///     clock.0 = clock.0.next()?; // any error type: it becomes `GraphError::TaskFailed`
///     Ok(())
/// });
/// g.task("move", |t| t.writes::<Positions>(), |ctx| {
///     ctx.write::<Positions>().map(|mut p| p.0.iter_mut().for_each(|x| *x += 1.0))
/// });
/// let mut graph = g.build()?;
///
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
/// let mut world = Resources::new();
/// world.insert(Clock(Tick::ZERO));
/// world.insert(Positions(vec![0.0; 100]));
///
/// for _ in 0..10 {
///     // ... input, async completions: mutate `world` between frames via `get_mut` ...
///     graph.run(&pool, &mut world)?;
/// }
/// assert_eq!(world.get::<Clock>().map(|c| c.0), Some(Tick::new(10)));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
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
    ///
    /// See the [`TaskConfig`] example.
    pub fn order(&self) -> impl Iterator<Item = &str> {
        self.order
            .iter()
            .filter_map(|&i| self.nodes.get(i).map(|n| n.name.as_str()))
    }

    /// "Earlier → later" edges after transitive reduction, in topological order of their sources.
    ///
    /// ```
    /// use pragma_graph::FrameGraph;
    ///
    /// struct A;
    ///
    /// let mut g = FrameGraph::builder();
    /// g.task("write", |t| t.writes::<A>(), |_| ());
    /// g.task("read 1", |t| t.reads::<A>().after("write"), |_| ());
    /// g.task("read 2", |t| t.reads::<A>().after("write"), |_| ());
    /// g.task("last", |t| t.after("read 1").after("read 2").after("write"), |_| ());
    /// let graph = g.build()?;
    /// // `write -> last` is implied by the other edges and is reduced away.
    /// assert_eq!(
    ///     graph.edges().collect::<Vec<_>>(),
    ///     [("write", "read 1"), ("write", "read 2"), ("read 1", "last"), ("read 2", "last")],
    /// );
    /// # Ok::<(), pragma_graph::GraphError>(())
    /// ```
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
    ///
    /// Call it from a thread outside the pool (the main loop): that thread runs graph tasks too
    /// while the frame is in flight. A failed frame leaves the graph usable: the next `run`
    /// starts from scratch.
    ///
    /// ```
    /// use pragma_graph::{FrameGraph, GraphError, Resources};
    /// use pragma_jobs::{PoolConfig, ThreadPool};
    ///
    /// struct Hp(i32);
    /// struct Log(Vec<&'static str>);
    ///
    /// let mut g = FrameGraph::builder();
    /// g.task("damage", |t| t.writes::<Hp>(), |ctx| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ///     let mut hp = ctx.write::<Hp>()?;
    ///     hp.0 -= 10;
    ///     if hp.0 < 0 {
    ///         return Err("dead".into());
    ///     }
    ///     Ok(())
    /// });
    /// g.task("log", |t| t.writes::<Log>().after("damage"), |ctx| {
    ///     ctx.write::<Log>().map(|mut l| l.0.push("hit"))
    /// });
    /// let mut graph = g.build()?;
    ///
    /// let pool = ThreadPool::new(PoolConfig::default().with_workers(1))?;
    /// let mut res = Resources::new();
    /// res.insert(Hp(15));
    /// res.insert(Log(Vec::new()));
    /// graph.run(&pool, &mut res)?;
    /// let failed = graph.run(&pool, &mut res); // hp goes below zero
    /// assert!(matches!(failed, Err(GraphError::TaskFailed { .. })));
    /// // "log" depends on "damage" and was skipped in the failed frame.
    /// assert_eq!(res.get::<Log>().map(|l| l.0.len()), Some(1));
    ///
    /// res.remove::<Log>();
    /// assert!(matches!(graph.run(&pool, &mut res), Err(GraphError::MissingResource { .. })));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
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
