//! Проверка описания графа и расчёт порядка: дубли, неизвестные зависимости, циклы,
//! неявные конфликты, стабильная топосортировка, транзитивная редукция рёбер.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use crate::GraphError;
use crate::resources::ResourceKey;

/// Наибольшее число задач в графе. Достижимость при сборке — битовая матрица `n × n`
/// (`n²/8` байт): на пределе это 32 МиБ, а не гигабайты на случайно большом входе.
pub const MAX_TASKS: usize = 16_384;

/// Наибольшее число рёбер до транзитивной редукции: явные `before`/`after` и рёбра режима
/// `ordered` (если все задачи пишут один ресурс, их `n²/2`). На пределе — 32 МиБ индексов.
pub const MAX_EDGES: usize = 1 << 22;

pub(crate) const TOO_MANY_TASKS: &str = "more tasks than pragma_graph::MAX_TASKS";
const TOO_MANY_EDGES: &str = "more edges than pragma_graph::MAX_EDGES";
const OUT_OF_MEMORY: &str = "out of memory while planning";

fn too_large(tasks: usize, reason: &'static str) -> GraphError {
    GraphError::TooLarge { tasks, reason }
}

/// Объявленный доступ задачи к ресурсу.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Access {
    pub(crate) key: ResourceKey,
    pub(crate) write: bool,
}

/// Явный порядок относительно другой задачи.
#[derive(Debug, Clone)]
pub(crate) enum Dependency {
    /// Эта задача — раньше названной.
    Before(String),
    /// Эта задача — позже названной.
    After(String),
}

/// Описание задачи для планировщика.
pub(crate) struct TaskSpec<'a> {
    pub(crate) name: &'a str,
    pub(crate) access: &'a [Access],
    pub(crate) deps: &'a [Dependency],
}

/// Результат планирования.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Стабильный топологический порядок: из готовых задач первой идёт зарегистрированная раньше.
    pub(crate) order: Vec<usize>,
    /// Прямые последователи каждой задачи после транзитивной редукции, по возрастанию индекса.
    pub(crate) successors: Vec<Vec<usize>>,
}

/// Счётчик рёбер под [`MAX_EDGES`]; рост списков — через `try_reserve`.
struct EdgeBudget {
    tasks: usize,
    edges: usize,
}

impl EdgeBudget {
    fn push(&mut self, succ: &mut [Vec<usize>], from: usize, to: usize) -> Result<(), GraphError> {
        if self.edges >= MAX_EDGES {
            return Err(too_large(self.tasks, TOO_MANY_EDGES));
        }
        if let Some(list) = succ.get_mut(from) {
            list.try_reserve(1)
                .map_err(|_| too_large(self.tasks, OUT_OF_MEMORY))?;
            list.push(to);
            self.edges = self.edges.saturating_add(1);
        }
        Ok(())
    }
}

/// Порядок проверок (и приоритет ошибок): размер ([`MAX_TASKS`]) → дубли имён → неизвестные
/// зависимости → циклы явных рёбер → неявные конфликты (в режиме `ordered` — циклы после
/// добавления рёбер по порядку объявления). Память, которая растёт со входом быстрее, чем
/// линейно (рёбра, матрица достижимости), выделяется через `try_reserve`: нехватка —
/// [`GraphError::TooLarge`], а не `abort`.
pub(crate) fn plan(tasks: &[TaskSpec<'_>], ordered: bool) -> Result<Plan, GraphError> {
    let n = tasks.len();
    if n > MAX_TASKS {
        return Err(too_large(n, TOO_MANY_TASKS));
    }
    let mut budget = EdgeBudget { tasks: n, edges: 0 };
    let mut index: HashMap<&str, usize> = HashMap::new();
    index
        .try_reserve(n)
        .map_err(|_| too_large(n, OUT_OF_MEMORY))?;
    for (i, task) in tasks.iter().enumerate() {
        if index.insert(task.name, i).is_some() {
            return Err(GraphError::DuplicateTask {
                task: task.name.to_owned(),
            });
        }
    }

    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, task) in tasks.iter().enumerate() {
        for dep in task.deps {
            let (other, before) = match dep {
                Dependency::Before(name) => (name, true),
                Dependency::After(name) => (name, false),
            };
            let Some(&j) = index.get(other.as_str()) else {
                return Err(GraphError::UnknownDependency {
                    task: task.name.to_owned(),
                    missing: other.clone(),
                });
            };
            let (from, to) = if before { (i, j) } else { (j, i) };
            budget.push(&mut succ, from, to)?;
        }
    }
    normalize(&mut succ);

    let mut order = topo(&succ).map_err(|cycle| cycle_error(tasks, &cycle))?;
    let mut reach = closure(&order, &succ)?;

    // Конфликтующие пары проверяются по явному порядку; в режиме `ordered` неупорядоченная
    // пара получает ребро от объявленной раньше к объявленной позже.
    let mut added = false;
    for (i, a) in tasks.iter().enumerate() {
        for (j, b) in tasks.iter().enumerate().skip(i.saturating_add(1)) {
            let Some(resource) = conflict(a.access, b.access) else {
                continue;
            };
            if reaches(&reach, i, j) || reaches(&reach, j, i) {
                continue;
            }
            if !ordered {
                return Err(GraphError::AmbiguousOrder {
                    a: a.name.to_owned(),
                    b: b.name.to_owned(),
                    resource: resource.to_owned(),
                });
            }
            budget.push(&mut succ, i, j)?;
            added = true;
        }
    }
    if added {
        normalize(&mut succ);
        order = topo(&succ).map_err(|cycle| cycle_error(tasks, &cycle))?;
        reach = closure(&order, &succ)?;
    }

    let successors = reduce(&succ, &reach);
    Ok(Plan { order, successors })
}

fn normalize(succ: &mut [Vec<usize>]) {
    for list in succ {
        list.sort_unstable();
        list.dedup();
    }
}

/// Имя первого ресурса `a`, по которому `a` и `b` конфликтуют (хотя бы одна пишет).
fn conflict(a: &[Access], b: &[Access]) -> Option<&'static str> {
    a.iter()
        .find(|x| {
            b.iter()
                .any(|y| y.key.id == x.key.id && (x.write || y.write))
        })
        .map(|x| x.key.name)
}

fn cycle_error(tasks: &[TaskSpec<'_>], cycle: &[usize]) -> GraphError {
    GraphError::Cycle {
        tasks: cycle
            .iter()
            .filter_map(|&i| tasks.get(i).map(|t| t.name.to_owned()))
            .collect(),
    }
}

/// Алгоритм Кана с выбором готовой вершины с наименьшим индексом. `Err` — найденный цикл.
fn topo(succ: &[Vec<usize>]) -> Result<Vec<usize>, Vec<usize>> {
    let n = succ.len();
    let mut indegree = vec![0_usize; n];
    for &v in succ.iter().flatten() {
        if let Some(d) = indegree.get_mut(v) {
            *d = d.saturating_add(1);
        }
    }
    let mut ready: BinaryHeap<Reverse<usize>> = indegree
        .iter()
        .enumerate()
        .filter(|&(_, &d)| d == 0)
        .map(|(i, _)| Reverse(i))
        .collect();
    let mut order = Vec::with_capacity(n);
    while let Some(Reverse(u)) = ready.pop() {
        order.push(u);
        for &v in succ.get(u).map_or(&[][..], Vec::as_slice) {
            if let Some(d) = indegree.get_mut(v) {
                *d = d.saturating_sub(1);
                if *d == 0 {
                    ready.push(Reverse(v));
                }
            }
        }
    }
    if order.len() == n {
        Ok(order)
    } else {
        Err(find_cycle(succ, &indegree))
    }
}

/// Цикл в остатке алгоритма Кана: у каждой оставшейся вершины есть предшественник среди
/// оставшихся, поэтому обратный обход от любой из них замыкается. Результат начинается
/// с вершины с наименьшим индексом — сообщение об ошибке воспроизводимо.
fn find_cycle(succ: &[Vec<usize>], indegree: &[usize]) -> Vec<usize> {
    let left = |i: usize| indegree.get(i).is_some_and(|&d| d > 0);
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); succ.len()];
    for (u, list) in succ.iter().enumerate().filter(|&(u, _)| left(u)) {
        for &v in list.iter().filter(|&&v| left(v)) {
            if let Some(p) = preds.get_mut(v) {
                p.push(u);
            }
        }
    }
    let Some(start) = (0..succ.len()).find(|&i| left(i)) else {
        return Vec::new();
    };
    let mut position: Vec<Option<usize>> = vec![None; succ.len()];
    let mut path = vec![start];
    if let Some(p) = position.get_mut(start) {
        *p = Some(0);
    }
    let mut current = start;
    loop {
        let Some(&pred) = preds.get(current).and_then(|p| p.iter().min()) else {
            // Недостижимо (см. выше); возвращаем обойдённую часть как есть.
            return path;
        };
        if let Some(at) = position.get(pred).copied().flatten() {
            // `path[at..]` идёт против рёбер, и `pred -> current` замыкает его.
            let mut cycle = path.split_off(at);
            cycle.reverse();
            if let Some(min_at) = cycle
                .iter()
                .enumerate()
                .min_by_key(|&(_, &v)| v)
                .map(|(k, _)| k)
            {
                cycle.rotate_left(min_at);
            }
            return cycle;
        }
        if let Some(p) = position.get_mut(pred) {
            *p = Some(path.len());
        }
        path.push(pred);
        current = pred;
    }
}

/// Множество вершин на битах.
#[derive(Clone)]
struct BitSet(Vec<u64>);

fn bit(i: usize) -> u64 {
    1_u64.wrapping_shl(u32::try_from(i % 64).unwrap_or(0))
}

impl BitSet {
    /// Пустое множество на `n` вершин; память — через `try_reserve`.
    fn try_new(n: usize) -> Option<Self> {
        let words = n.div_ceil(64);
        let mut bits = Vec::new();
        bits.try_reserve_exact(words).ok()?;
        bits.resize(words, 0);
        Some(Self(bits))
    }

    fn insert(&mut self, i: usize) {
        if let Some(word) = self.0.get_mut(i / 64) {
            *word |= bit(i);
        }
    }

    fn contains(&self, i: usize) -> bool {
        self.0.get(i / 64).is_some_and(|word| word & bit(i) != 0)
    }

    fn union_with(&mut self, other: &Self) {
        for (a, b) in self.0.iter_mut().zip(&other.0) {
            *a |= b;
        }
    }
}

/// Достижимость: `reach[u]` — все вершины, до которых есть путь из `u`. Матрица `n × n` бит —
/// самое большое выделение сборки, поэтому оно fallible.
fn closure(order: &[usize], succ: &[Vec<usize>]) -> Result<Vec<BitSet>, GraphError> {
    let n = succ.len();
    let oom = || too_large(n, OUT_OF_MEMORY);
    let mut reach: Vec<BitSet> = Vec::new();
    reach.try_reserve_exact(n).map_err(|_| oom())?;
    // Пустые множества без выделения; каждое заполняется ровно один раз ниже.
    reach.resize_with(n, || BitSet(Vec::new()));
    for &u in order.iter().rev() {
        let mut acc = BitSet::try_new(n).ok_or_else(oom)?;
        for &v in succ.get(u).map_or(&[][..], Vec::as_slice) {
            acc.insert(v);
            if let Some(r) = reach.get(v) {
                acc.union_with(r);
            }
        }
        if let Some(r) = reach.get_mut(u) {
            *r = acc;
        }
    }
    Ok(reach)
}

fn reaches(reach: &[BitSet], from: usize, to: usize) -> bool {
    reach.get(from).is_some_and(|r| r.contains(to))
}

/// Транзитивная редукция: ребро `u -> v` лишнее, если `v` достижима через другого прямого
/// последователя `u`. Достижимость не меняется, а счётчиков на кадр становится меньше.
fn reduce(succ: &[Vec<usize>], reach: &[BitSet]) -> Vec<Vec<usize>> {
    succ.iter()
        .map(|list| {
            list.iter()
                .copied()
                .filter(|&v| !list.iter().any(|&w| w != v && reaches(reach, w, v)))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct A;
    struct B;

    fn read<T: 'static>() -> Access {
        Access {
            key: ResourceKey::of::<T>(),
            write: false,
        }
    }

    fn write<T: 'static>() -> Access {
        Access {
            key: ResourceKey::of::<T>(),
            write: true,
        }
    }

    fn spec<'a>(name: &'a str, access: &'a [Access], deps: &'a [Dependency]) -> TaskSpec<'a> {
        TaskSpec { name, access, deps }
    }

    #[test]
    fn order_is_stable_by_registration() {
        let tasks = [
            spec("c", &[], &[]),
            spec("a", &[], &[]),
            spec("b", &[], &[]),
        ];
        let plan = plan(&tasks, false).unwrap();
        assert_eq!(plan.order, vec![0, 1, 2]);
    }

    #[test]
    fn explicit_edges_and_reduction() {
        let after_a = [Dependency::After("a".into())];
        let after_ab = [Dependency::After("a".into()), Dependency::After("b".into())];
        let tasks = [
            spec("a", &[], &[]),
            spec("b", &[], &after_a),
            spec("c", &[], &after_ab),
        ];
        let plan = plan(&tasks, false).unwrap();
        assert_eq!(plan.order, vec![0, 1, 2]);
        // a -> c лишнее: c достижима через b.
        assert_eq!(plan.successors, vec![vec![1], vec![2], vec![]]);
    }

    #[test]
    fn reversed_explicit_order_wins_over_registration() {
        let before_a = [Dependency::Before("a".into())];
        let tasks = [spec("a", &[], &[]), spec("b", &[], &before_a)];
        assert_eq!(plan(&tasks, false).unwrap().order, vec![1, 0]);
    }

    #[test]
    fn errors_have_names_and_priority() {
        let dup = [spec("a", &[], &[]), spec("a", &[], &[])];
        assert_eq!(
            plan(&dup, false),
            Err(GraphError::DuplicateTask { task: "a".into() })
        );

        let unknown = [Dependency::After("nope".into())];
        let tasks = [spec("a", &[], &unknown)];
        assert_eq!(
            plan(&tasks, false),
            Err(GraphError::UnknownDependency {
                task: "a".into(),
                missing: "nope".into()
            })
        );

        let after_c = [Dependency::After("c".into())];
        let after_a = [Dependency::After("a".into())];
        let after_b = [Dependency::After("b".into())];
        let tasks = [
            spec("x", &[], &[]),
            spec("b", &[], &after_a),
            spec("c", &[], &after_b),
            spec("a", &[], &after_c),
        ];
        assert_eq!(
            plan(&tasks, false),
            Err(GraphError::Cycle {
                tasks: vec!["b".into(), "c".into(), "a".into()]
            })
        );

        let self_loop = [Dependency::After("s".into())];
        let tasks = [spec("s", &[], &self_loop)];
        assert_eq!(
            plan(&tasks, false),
            Err(GraphError::Cycle {
                tasks: vec!["s".into()]
            })
        );
    }

    #[test]
    fn conflicts_need_order() {
        let w = [write::<A>()];
        let r = [read::<A>(), read::<B>()];
        let tasks = [spec("w", &w, &[]), spec("r", &r, &[])];
        let Err(GraphError::AmbiguousOrder { a, b, resource }) = plan(&tasks, false) else {
            panic!("expected AmbiguousOrder");
        };
        assert_eq!((a.as_str(), b.as_str()), ("w", "r"));
        assert!(resource.ends_with("A"));

        // Читатели не конфликтуют.
        let tasks = [spec("r1", &r, &[]), spec("r2", &r, &[])];
        assert_eq!(
            plan(&tasks, false).unwrap().successors,
            vec![vec![], vec![]]
        );

        // Ordered: порядок объявления.
        let tasks = [spec("w", &w, &[]), spec("r", &r, &[])];
        assert_eq!(
            plan(&tasks, true).unwrap().successors,
            vec![vec![1], vec![]]
        );

        // Ordered: явный обратный порядок не перебивается.
        let before_w = [Dependency::Before("w".into())];
        let tasks = [spec("w", &w, &[]), spec("r", &r, &before_w)];
        let p = plan(&tasks, true).unwrap();
        assert_eq!(p.order, vec![1, 0]);
        assert_eq!(p.successors, vec![vec![], vec![0]]);
    }

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("t{i}")).collect()
    }

    #[test]
    fn task_limit_is_an_error_before_any_quadratic_work() {
        let names = names(MAX_TASKS + 1);
        let tasks: Vec<_> = names.iter().map(|n| spec(n, &[], &[])).collect();
        assert_eq!(
            plan(&tasks, false),
            Err(GraphError::TooLarge {
                tasks: MAX_TASKS + 1,
                reason: TOO_MANY_TASKS
            })
        );
        // Ровно на пределе граф строится (матрица — 32 МиБ).
        assert_eq!(
            plan(&tasks[..MAX_TASKS], false).unwrap().order.len(),
            MAX_TASKS
        );
    }

    #[test]
    fn ordered_edges_are_capped() {
        // Все пишут один ресурс: режим `ordered` добавил бы n²/2 рёбер.
        let w = [write::<A>()];
        let n = 3_000; // 4.5M пар > MAX_EDGES
        let names = names(n);
        let tasks: Vec<_> = names.iter().map(|name| spec(name, &w, &[])).collect();
        assert_eq!(
            plan(&tasks, true),
            Err(GraphError::TooLarge {
                tasks: n,
                reason: TOO_MANY_EDGES
            })
        );
    }

    #[test]
    fn ordered_mode_can_close_a_cycle() {
        let w = [write::<A>()];
        let before_a = [Dependency::Before("a".into())];
        // Явно c -> a; по объявлению a -> b -> c: цикл.
        let tasks = [
            spec("a", &w, &[]),
            spec("b", &w, &[]),
            spec("c", &w, &before_a),
        ];
        assert!(matches!(plan(&tasks, true), Err(GraphError::Cycle { .. })));
    }
}
