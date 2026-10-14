use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use pragma_base::tracing;
use pragma_jobs::{Priority, ThreadPool};
use pragma_sync::ArrivalCounter;

use crate::GraphError;
use crate::context::{TaskContext, TaskResult};
use crate::plan::{self, Access, Dependency, TaskSpec};
use crate::resources::{ResourceKey, Resources};

type System = Box<dyn Fn(&TaskContext<'_>) -> Result<(), GraphError> + Send + Sync>;

/// Объявление задачи: доступ к ресурсам и явный порядок.
///
/// Передаётся в замыкание-настройку [`FrameGraphBuilder::task`]:
/// `|t| t.reads::<Input>().writes::<Bodies>().after("input")`.
#[derive(Debug, Default)]
#[must_use]
pub struct TaskConfig {
    access: Vec<Access>,
    deps: Vec<Dependency>,
}

impl TaskConfig {
    /// Задача читает ресурс `T`.
    pub fn reads<T: Send + Sync + 'static>(self) -> Self {
        self.declare(ResourceKey::of::<T>(), false)
    }

    /// Задача пишет ресурс `T` (запись включает чтение).
    pub fn writes<T: Send + Sync + 'static>(self) -> Self {
        self.declare(ResourceKey::of::<T>(), true)
    }

    /// Задача исполняется раньше задачи `task`.
    pub fn before(mut self, task: impl Into<String>) -> Self {
        self.deps.push(Dependency::Before(task.into()));
        self
    }

    /// Задача исполняется позже задачи `task`.
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

/// Построитель графа кадра (см. [`FrameGraph::builder`]).
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
    /// Режим `ordered`: конфликтующие задачи без явного порядка исполняются в порядке
    /// объявления, а не дают [`GraphError::AmbiguousOrder`]. Явные `before`/`after` важнее
    /// порядка объявления; если вместе они замыкают цикл — [`GraphError::Cycle`].
    pub fn ordered(mut self) -> Self {
        self.ordered = true;
        self
    }

    /// Регистрирует задачу `name`: `configure` объявляет доступ и порядок, `system` — работа.
    ///
    /// `system` вызывается раз в кадр; возвращает `()` или `Result<(), E>` (см. [`TaskResult`]).
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

    /// Проверяет описание и строит граф: дубли имён ([`GraphError::DuplicateTask`]),
    /// неизвестные зависимости ([`GraphError::UnknownDependency`]), циклы
    /// ([`GraphError::Cycle`]) и конфликты доступа без порядка ([`GraphError::AmbiguousOrder`]).
    /// Граф больше [`MAX_TASKS`](crate::MAX_TASKS) задач или [`MAX_EDGES`](crate::MAX_EDGES)
    /// рёбер, как и нехватка памяти при планировании, — [`GraphError::TooLarge`].
    pub fn build(self) -> Result<FrameGraph, GraphError> {
        // До любых выделений по числу задач (reliability §1).
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
    /// Прямые последователи (после транзитивной редукции), по возрастанию индекса.
    pub(crate) successors: Vec<usize>,
    /// Черновик на кадр: последователи, ставшие готовыми после этой задачи. Пишет только
    /// поток, исполнивший задачу (каждая задача исполняется раз за кадр).
    ready: Box<[AtomicUsize]>,
    indegree: usize,
}

/// Граф задач кадра: строится один раз ([`FrameGraph::builder`]), исполняется каждый кадр
/// ([`FrameGraph::run`]) без аллокаций.
pub struct FrameGraph {
    pub(crate) nodes: Vec<Node>,
    order: Vec<usize>,
    /// Задачи без предшественников (индексы; атомики — чтобы делить код с `Node::ready`).
    roots: Box<[AtomicUsize]>,
    /// Сколько предшественников задачи ещё не завершилось в текущем кадре.
    remaining: Box<[ArrivalCounter]>,
    /// Все ресурсы графа и первая задача, объявившая каждый.
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
    /// Новый построитель.
    pub fn builder() -> FrameGraphBuilder {
        FrameGraphBuilder::default()
    }

    /// Число задач.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// В графе нет задач.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Имена задач в стабильном топологическом порядке: из готовых задач первой идёт
    /// зарегистрированная раньше. На одном воркере задачи исполняются не обязательно в этом
    /// порядке, но любой реальный порядок совместим с теми же рёбрами.
    pub fn order(&self) -> impl Iterator<Item = &str> {
        self.order
            .iter()
            .filter_map(|&i| self.nodes.get(i).map(|n| n.name.as_str()))
    }

    /// Рёбра «раньше → позже» после транзитивной редукции, в топологическом порядке начала.
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

    /// Исполняет кадр: каждая задача — ровно один раз, после всех своих предшественников;
    /// независимые задачи — параллельно на `pool`.
    ///
    /// Перед стартом проверяет, что в `resources` есть все объявленные ресурсы
    /// ([`GraphError::MissingResource`]). Ошибка или паника задачи не останавливает независимые
    /// ветви, но зависимые от неё задачи в этом кадре не запускаются; возвращается ошибка задачи
    /// с наименьшим индексом регистрации. В установившемся режиме не аллоцирует.
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
        // Одна область на кадр: внешний поток входит в пул один раз, а не на каждый `join`.
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

/// Состояние одного кадра.
struct Run<'a> {
    nodes: &'a [Node],
    remaining: &'a [ArrivalCounter],
    pool: &'a ThreadPool,
    resources: &'a Resources,
    /// Ошибка задачи с наименьшим индексом (`usize::MAX` — ошибка вне задач).
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

    /// Запускает готовые задачи: половину — здесь, вторую отдаёт ворам через `join`
    /// (`join` не аллоцирует, в отличие от `spawn`).
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

    /// Исполняет задачу и освобождает последователей. Цепочку из одного готового
    /// последователя продолжает циклом, без роста стека.
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
                // Последний прибывший предшественник видит записи всех остальных и публикует их
                // вместе со своими той задаче, которую запускает.
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
        // Поля span вычисляются, только если уровень DEBUG включён.
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
