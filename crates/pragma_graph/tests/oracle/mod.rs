//! Оракул графа для property-тестов (`tests/prop.rs`) и fuzz (`fuzz/fuzz_targets/graph_build.rs`):
//! произвольное описание → либо ожидаемая ошибка, либо порядок и рёбра, совпадающие с
//! независимой (наивной) реализацией. При исполнении проверяет, что рёбра соблюдаются, каждая
//! задача выполняется ровно один раз, а взятие объявленных ресурсов никогда не конфликтует.
// Тестовый код: паника = провал проверки, размеры входа малы.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::needless_range_loop,
    dead_code
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pragma_graph::{FrameGraph, GraphError, Res, ResMut, Resources, TaskConfig, TaskContext};
use pragma_jobs::ThreadPool;

/// Число типов ресурсов в описаниях.
pub const RESOURCES: usize = 4;
/// Максимум задач в описании.
pub const MAX_TASKS: usize = 12;

/// Ресурс номер `N`: счётчик записей.
#[derive(Debug)]
pub struct R<const N: usize>(pub u64);

/// Описание задачи.
#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub name: String,
    /// Биты `0..RESOURCES`: читаемые ресурсы.
    pub reads: u8,
    /// Биты `0..RESOURCES`: записываемые ресурсы (запись важнее чтения).
    pub writes: u8,
    /// `(true, x)` — `before(x)`, `(false, x)` — `after(x)`, в порядке объявления.
    pub deps: Vec<(bool, String)>,
}

/// Описание графа.
#[derive(Debug, Clone)]
pub struct Spec {
    pub ordered: bool,
    pub tasks: Vec<TaskSpec>,
}

impl Spec {
    /// Декодирует описание из произвольных байтов (недостающие байты — нули). Дубли имён и
    /// неизвестные зависимости редки, чтобы большинство входов доходило до проверки порядка.
    pub fn from_bytes(data: &[u8]) -> Self {
        let mut it = data.iter().copied();
        let mut next = move || it.next().unwrap_or(0);
        let ordered = next() & 1 == 1;
        let n = usize::from(next()) % (MAX_TASKS + 1);
        let mut tasks = Vec::with_capacity(n);
        for i in 0..n {
            let name = if next() == 0xFF && i > 0 {
                format!("t{}", usize::from(next()) % i)
            } else {
                format!("t{i}")
            };
            let access = next();
            let mask = (1_u8 << RESOURCES) - 1;
            let mut deps = Vec::new();
            for _ in 0..next() % 3 {
                let d = next();
                let target = d >> 1;
                let other = if target >= 120 {
                    String::from("ghost")
                } else {
                    format!("t{}", usize::from(target) % n)
                };
                deps.push((d & 1 == 1, other));
            }
            tasks.push(TaskSpec {
                name,
                reads: access & mask,
                writes: (access >> 4) & mask,
                deps,
            });
        }
        Self { ordered, tasks }
    }
}

fn bit(mask: u8, k: usize) -> bool {
    mask & (1 << k) != 0
}

pub fn resource_name(k: usize) -> &'static str {
    match k {
        0 => std::any::type_name::<R<0>>(),
        1 => std::any::type_name::<R<1>>(),
        2 => std::any::type_name::<R<2>>(),
        _ => std::any::type_name::<R<3>>(),
    }
}

fn declare(mut t: TaskConfig, task: &TaskSpec) -> TaskConfig {
    for k in 0..RESOURCES {
        let write = bit(task.writes, k);
        if !write && !bit(task.reads, k) {
            continue;
        }
        t = match (k, write) {
            (0, true) => t.writes::<R<0>>(),
            (0, false) => t.reads::<R<0>>(),
            (1, true) => t.writes::<R<1>>(),
            (1, false) => t.reads::<R<1>>(),
            (2, true) => t.writes::<R<2>>(),
            (2, false) => t.reads::<R<2>>(),
            (_, true) => t.writes::<R<3>>(),
            (_, false) => t.reads::<R<3>>(),
        };
    }
    for (before, other) in &task.deps {
        t = if *before {
            t.before(other.clone())
        } else {
            t.after(other.clone())
        };
    }
    t
}

/// Взятый ресурс (держим все на время «работы», чтобы конфликты успели проявиться).
enum Guard<'a> {
    R0(Res<'a, R<0>>),
    R1(Res<'a, R<1>>),
    R2(Res<'a, R<2>>),
    R3(Res<'a, R<3>>),
    W0(ResMut<'a, R<0>>),
    W1(ResMut<'a, R<1>>),
    W2(ResMut<'a, R<2>>),
    W3(ResMut<'a, R<3>>),
}

fn take<'a>(ctx: &TaskContext<'a>, k: usize, write: bool) -> Result<Guard<'a>, GraphError> {
    Ok(match (k, write) {
        (0, false) => Guard::R0(ctx.read()?),
        (1, false) => Guard::R1(ctx.read()?),
        (2, false) => Guard::R2(ctx.read()?),
        (_, false) => Guard::R3(ctx.read()?),
        (0, true) => Guard::W0(ctx.write()?),
        (1, true) => Guard::W1(ctx.write()?),
        (2, true) => Guard::W2(ctx.write()?),
        (_, true) => Guard::W3(ctx.write()?),
    })
}

fn bump(guard: &mut Guard<'_>) {
    match guard {
        Guard::W0(r) => r.0 += 1,
        Guard::W1(r) => r.0 += 1,
        Guard::W2(r) => r.0 += 1,
        Guard::W3(r) => r.0 += 1,
        Guard::R0(_) | Guard::R1(_) | Guard::R2(_) | Guard::R3(_) => {}
    }
}

/// Отметки исполнения задачи: момент начала и конца по общим «часам».
#[derive(Default)]
struct Trace {
    clock: AtomicUsize,
    start: Vec<AtomicUsize>,
    end: Vec<AtomicUsize>,
    runs: Vec<AtomicUsize>,
}

/// Ожидаемый исход сборки.
enum Expected {
    Duplicate(String),
    Unknown {
        task: String,
        missing: String,
    },
    /// Цикл; рёбра, по которым он должен проходить.
    Cycle(Vec<Vec<bool>>),
    Ambiguous {
        a: String,
        b: String,
        resource: &'static str,
    },
    Ok {
        order: Vec<usize>,
        reach: Vec<Vec<bool>>,
    },
}

fn closure(n: usize, edges: &[Vec<bool>]) -> Vec<Vec<bool>> {
    let mut reach = edges.to_vec();
    for k in 0..n {
        for i in 0..n {
            if reach[i][k] {
                for j in 0..n {
                    if reach[k][j] {
                        reach[i][j] = true;
                    }
                }
            }
        }
    }
    reach
}

fn has_cycle(n: usize, reach: &[Vec<bool>]) -> bool {
    (0..n).any(|i| reach[i][i])
}

/// Kahn с минимальным готовым индексом — квадратичная эталонная версия.
fn stable_order(n: usize, edges: &[Vec<bool>]) -> Vec<usize> {
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        let next = (0..n)
            .find(|&v| !done[v] && (0..n).all(|u| !edges[u][v] || done[u]))
            .unwrap();
        done[next] = true;
        order.push(next);
    }
    order
}

fn expected(spec: &Spec) -> Expected {
    let tasks = &spec.tasks;
    let n = tasks.len();
    for (i, t) in tasks.iter().enumerate() {
        if tasks[..i].iter().any(|p| p.name == t.name) {
            return Expected::Duplicate(t.name.clone());
        }
    }
    let index = |name: &str| tasks.iter().position(|t| t.name == name);
    let mut edges = vec![vec![false; n]; n];
    for (i, t) in tasks.iter().enumerate() {
        for (before, other) in &t.deps {
            let Some(j) = index(other) else {
                return Expected::Unknown {
                    task: t.name.clone(),
                    missing: other.clone(),
                };
            };
            if *before {
                edges[i][j] = true;
            } else {
                edges[j][i] = true;
            }
        }
    }
    let reach = closure(n, &edges);
    if has_cycle(n, &reach) {
        return Expected::Cycle(edges);
    }
    for i in 0..n {
        for j in i + 1..n {
            let (a, b) = (&tasks[i], &tasks[j]);
            let any_a = a.reads | a.writes;
            let any_b = b.reads | b.writes;
            let Some(k) =
                (0..RESOURCES).find(|&k| bit(any_a & any_b, k) && bit(a.writes | b.writes, k))
            else {
                continue;
            };
            if reach[i][j] || reach[j][i] {
                continue;
            }
            if !spec.ordered {
                return Expected::Ambiguous {
                    a: a.name.clone(),
                    b: b.name.clone(),
                    resource: resource_name(k),
                };
            }
            edges[i][j] = true;
        }
    }
    let reach = closure(n, &edges);
    if has_cycle(n, &reach) {
        return Expected::Cycle(edges);
    }
    Expected::Ok {
        order: stable_order(n, &edges),
        reach,
    }
}

/// Строит граф по описанию и сверяет с оракулом. С `pool` — ещё и исполняет `frames` кадров.
pub fn check(spec: &Spec, pool: Option<&ThreadPool>, frames: usize) {
    let n = spec.tasks.len();
    let trace = Arc::new(Trace {
        clock: AtomicUsize::new(0),
        start: (0..n).map(|_| AtomicUsize::new(0)).collect(),
        end: (0..n).map(|_| AtomicUsize::new(0)).collect(),
        runs: (0..n).map(|_| AtomicUsize::new(0)).collect(),
    });
    let mut g = if spec.ordered {
        FrameGraph::builder().ordered()
    } else {
        FrameGraph::builder()
    };
    for (i, task) in spec.tasks.iter().enumerate() {
        let (reads, writes) = (task.reads, task.writes);
        let trace = Arc::clone(&trace);
        g.task(
            task.name.clone(),
            |t| declare(t, task),
            move |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
                trace.start[i].store(trace.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                let mut guards = Vec::new();
                for k in 0..RESOURCES {
                    let write = bit(writes, k);
                    if write || bit(reads, k) {
                        guards.push(take(ctx, k, write)?);
                    }
                }
                guards.iter_mut().for_each(bump);
                std::thread::yield_now();
                drop(guards);
                trace.runs[i].fetch_add(1, Ordering::SeqCst);
                trace.end[i].store(trace.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                Ok(())
            },
        );
    }
    let built = g.build();
    let names: Vec<&str> = spec.tasks.iter().map(|t| t.name.as_str()).collect();
    let index = |name: &str| names.iter().position(|n| *n == name).unwrap();
    let (order, reach, mut graph) = match (expected(spec), built) {
        (Expected::Duplicate(task), Err(err)) => {
            assert_eq!(err, GraphError::DuplicateTask { task });
            return;
        }
        (Expected::Unknown { task, missing }, Err(err)) => {
            assert_eq!(err, GraphError::UnknownDependency { task, missing });
            return;
        }
        (Expected::Cycle(edges), Err(GraphError::Cycle { tasks })) => {
            assert!(!tasks.is_empty());
            for (k, from) in tasks.iter().enumerate() {
                let to = &tasks[(k + 1) % tasks.len()];
                assert!(
                    edges[index(from)][index(to)],
                    "reported cycle {tasks:?} uses missing edge {from} -> {to}"
                );
            }
            return;
        }
        (Expected::Ambiguous { a, b, resource }, Err(err)) => {
            assert_eq!(
                err,
                GraphError::AmbiguousOrder {
                    a,
                    b,
                    resource: resource.into()
                }
            );
            return;
        }
        (Expected::Ok { order, reach }, Ok(graph)) => (order, reach, graph),
        (_, other) => panic!("oracle and builder disagree for {spec:?}: got {other:?}"),
    };

    // Стабильный порядок совпадает с эталоном.
    let actual: Vec<usize> = graph.order().map(index).collect();
    assert_eq!(actual, order, "order for {spec:?}");

    // Рёбра графа (после редукции) дают ту же достижимость, что и эталонные.
    let mut edges = vec![vec![false; n]; n];
    for (from, to) in graph.edges() {
        edges[index(from)][index(to)] = true;
    }
    assert_eq!(closure(n, &edges), reach, "reachability for {spec:?}");
    assert_eq!(
        graph.to_dot().matches(" -> ").count(),
        graph.edges().count()
    );

    let Some(pool) = pool else {
        return;
    };
    let mut res = Resources::new();
    res.insert(R::<0>(0));
    res.insert(R::<1>(0));
    res.insert(R::<2>(0));
    res.insert(R::<3>(0));
    for frame in 1..=frames {
        graph.run(pool, &mut res).unwrap();
        for i in 0..n {
            assert_eq!(
                trace.runs[i].load(Ordering::SeqCst),
                frame,
                "task {i} runs once per frame"
            );
        }
        for (from, to) in graph.edges() {
            let (u, v) = (index(from), index(to));
            assert!(
                trace.end[u].load(Ordering::SeqCst) < trace.start[v].load(Ordering::SeqCst),
                "{from} must finish before {to} starts"
            );
        }
    }
    // Каждая пишущая задача увеличила свой ресурс раз за кадр.
    let writers = |k: usize| spec.tasks.iter().filter(|t| bit(t.writes, k)).count() as u64;
    let frames = frames as u64;
    assert_eq!(res.get::<R<0>>().unwrap().0, writers(0) * frames);
    assert_eq!(res.get::<R<1>>().unwrap().0, writers(1) * frames);
    assert_eq!(res.get::<R<2>>().unwrap().0, writers(2) * frames);
    assert_eq!(res.get::<R<3>>().unwrap().0, writers(3) * frames);
}
