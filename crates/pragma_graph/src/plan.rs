//! Validating the graph description and computing the order: duplicates, unknown dependencies,
//! cycles, implicit conflicts, a stable topological sort, transitive reduction of edges.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use crate::GraphError;
use crate::resources::ResourceKey;

/// Maximum number of tasks in a graph. Reachability at build time is an `n × n` bit matrix
/// (`n²/8` bytes): at the limit that is 32 MiB, not gigabytes on an accidentally huge input.
///
/// ```
/// use pragma_graph::{FrameGraph, GraphError, MAX_TASKS};
///
/// let mut g = FrameGraph::builder();
/// for i in 0..=MAX_TASKS {
///     g.task(format!("t{i}"), |t| t, |_| ());
/// }
/// assert!(matches!(g.build(), Err(GraphError::TooLarge { .. })));
/// ```
pub const MAX_TASKS: usize = 16_384;

/// Maximum number of edges before transitive reduction: explicit `before`/`after` and the edges of
/// the `ordered` mode (if all tasks write one resource, there are `n²/2` of them). At the limit, 32 MiB of indices.
pub const MAX_EDGES: usize = 1 << 22;

pub(crate) const TOO_MANY_TASKS: &str = "more tasks than pragma_graph::MAX_TASKS";
const TOO_MANY_EDGES: &str = "more edges than pragma_graph::MAX_EDGES";
const OUT_OF_MEMORY: &str = "out of memory while planning";

fn too_large(tasks: usize, reason: &'static str) -> GraphError {
    GraphError::TooLarge { tasks, reason }
}

/// A task's declared access to a resource.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Access {
    pub(crate) key: ResourceKey,
    pub(crate) write: bool,
}

/// An explicit order relative to another task.
#[derive(Debug, Clone)]
pub(crate) enum Dependency {
    /// This task comes before the named one.
    Before(String),
    /// This task comes after the named one.
    After(String),
}

/// A task description for the planner.
pub(crate) struct TaskSpec<'a> {
    pub(crate) name: &'a str,
    pub(crate) access: &'a [Access],
    pub(crate) deps: &'a [Dependency],
}

/// The result of planning.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// A stable topological order: among ready tasks, the one registered earlier comes first.
    pub(crate) order: Vec<usize>,
    /// The direct successors of each task after transitive reduction, in ascending index order.
    pub(crate) successors: Vec<Vec<usize>>,
}

/// An edge counter capped by [`MAX_EDGES`]; lists grow via `try_reserve`.
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

/// The order of checks (and error precedence): size ([`MAX_TASKS`]) → duplicate names → unknown
/// dependencies → cycles of explicit edges → implicit conflicts (in `ordered` mode, cycles after
/// adding edges in declaration order). Memory that grows faster than linearly with the input
/// (edges, the reachability matrix) is allocated via `try_reserve`: running out is
/// [`GraphError::TooLarge`], not `abort`.
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

    // Conflicting pairs are checked against the explicit order; in `ordered` mode an unordered
    // pair gets an edge from the one declared earlier to the one declared later.
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

/// The name of the first resource of `a` on which `a` and `b` conflict (at least one of them writes).
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

/// Kahn's algorithm, picking the ready vertex with the lowest index. `Err` is the cycle found.
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

/// A cycle in what Kahn's algorithm left over: every remaining vertex has a predecessor among
/// the remaining ones, so a backward walk from any of them closes a loop. The result starts
/// with the lowest-index vertex, so the error message is reproducible.
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
            // Unreachable (see above); return the part walked so far as is.
            return path;
        };
        if let Some(at) = position.get(pred).copied().flatten() {
            // `path[at..]` runs against the edges, and `pred -> current` closes it.
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

/// A bit set of vertices.
#[derive(Clone)]
struct BitSet(Vec<u64>);

fn bit(i: usize) -> u64 {
    1_u64.wrapping_shl(u32::try_from(i % 64).unwrap_or(0))
}

impl BitSet {
    /// An empty set for `n` vertices; memory via `try_reserve`.
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

/// Reachability: `reach[u]` is every vertex reachable from `u`. The `n × n` bit matrix is the
/// largest allocation of the build, so it is fallible.
fn closure(order: &[usize], succ: &[Vec<usize>]) -> Result<Vec<BitSet>, GraphError> {
    let n = succ.len();
    let oom = || too_large(n, OUT_OF_MEMORY);
    let mut reach: Vec<BitSet> = Vec::new();
    reach.try_reserve_exact(n).map_err(|_| oom())?;
    // Empty sets without allocation; each is filled exactly once below.
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

/// Transitive reduction: an edge `u -> v` is redundant if `v` is reachable through another direct
/// successor of `u`. Reachability does not change, and there are fewer counters per frame.
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
        // a -> c is redundant: c is reachable through b.
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

        // Readers do not conflict.
        let tasks = [spec("r1", &r, &[]), spec("r2", &r, &[])];
        assert_eq!(
            plan(&tasks, false).unwrap().successors,
            vec![vec![], vec![]]
        );

        // Ordered: declaration order.
        let tasks = [spec("w", &w, &[]), spec("r", &r, &[])];
        assert_eq!(
            plan(&tasks, true).unwrap().successors,
            vec![vec![1], vec![]]
        );

        // Ordered: an explicit reverse order is not overridden.
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
        // Exactly at the limit the graph builds (the matrix is 32 MiB).
        assert_eq!(
            plan(&tasks[..MAX_TASKS], false).unwrap().order.len(),
            MAX_TASKS
        );
    }

    #[test]
    fn ordered_edges_are_capped() {
        // Everyone writes one resource: the `ordered` mode would add n²/2 edges.
        let w = [write::<A>()];
        let n = 3_000; // 4.5M pairs > MAX_EDGES
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
        // Explicitly c -> a; by declaration a -> b -> c: a cycle.
        let tasks = [
            spec("a", &w, &[]),
            spec("b", &w, &[]),
            spec("c", &w, &before_a),
        ];
        assert!(matches!(plan(&tasks, true), Err(GraphError::Cycle { .. })));
    }
}
