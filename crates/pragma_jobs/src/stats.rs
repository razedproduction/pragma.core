use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pragma_sync::__private::CachePadded;

use crate::Priority;

/// Counters of one pool thread. Each set sits on its own cache line and is written by one thread
/// (a worker or the owner of an external slot), so an increment is a `load` + `store` without an RMW
/// (`Relaxed`: metrics order nothing and do not affect task results).
#[derive(Default)]
pub(crate) struct Counters {
    executed: AtomicU64,
    stolen: AtomicU64,
    parks: AtomicU64,
    idle_ns: AtomicU64,
    parked_ns: AtomicU64,
}

fn add_exclusive(counter: &AtomicU64, by: u64) {
    counter.store(
        counter.load(Ordering::Relaxed).wrapping_add(by),
        Ordering::Relaxed,
    );
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl Counters {
    pub(crate) fn executed(&self) {
        add_exclusive(&self.executed, 1);
    }

    pub(crate) fn stolen(&self) {
        add_exclusive(&self.stolen, 1);
    }

    pub(crate) fn idle(&self, d: Duration) {
        add_exclusive(&self.idle_ns, nanos(d));
    }

    pub(crate) fn parked(&self, d: Duration) {
        add_exclusive(&self.parks, 1);
        add_exclusive(&self.parked_ns, nanos(d));
    }

    /// For a set written by several threads (external threads without a slot).
    pub(crate) fn executed_shared(&self) {
        self.executed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn stolen_shared(&self) {
        self.stolen.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self, queued: usize) -> ThreadStats {
        ThreadStats {
            executed: self.executed.load(Ordering::Relaxed),
            stolen: self.stolen.load(Ordering::Relaxed),
            parks: self.parks.load(Ordering::Relaxed),
            idle: Duration::from_nanos(self.idle_ns.load(Ordering::Relaxed)),
            parked: Duration::from_nanos(self.parked_ns.load(Ordering::Relaxed)),
            queued,
        }
    }
}

/// Counters of all pool threads: `0..workers` are the workers, then the external slots, and the last
/// one is for external threads without a slot.
pub(crate) type CounterSet = Box<[CachePadded<Counters>]>;

pub(crate) fn counter_set(threads: usize) -> CounterSet {
    (0..threads.saturating_add(1))
        .map(|_| CachePadded::new(Counters::default()))
        .collect()
}

/// A metrics snapshot of one thread (or a group of external threads), see [`PoolStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadStats {
    executed: u64,
    stolen: u64,
    parks: u64,
    idle: Duration,
    parked: Duration,
    queued: usize,
}

impl ThreadStats {
    /// How many tasks the thread took for execution (its own, from global queues, and stolen).
    #[must_use]
    pub fn executed(&self) -> u64 {
        self.executed
    }

    /// How many tasks were stolen from other threads' local queues.
    #[must_use]
    pub fn stolen(&self) -> u64 {
        self.stolen
    }

    /// How many times the worker fell asleep (parked) without work.
    #[must_use]
    pub fn parks(&self) -> u64 {
        self.parks
    }

    /// Time without work: searching (spin) and sleeping. Workers only.
    #[must_use]
    pub fn idle(&self) -> Duration {
        self.idle
    }

    /// The part of [`idle`](Self::idle) spent parked.
    #[must_use]
    pub fn parked(&self) -> Duration {
        self.parked
    }

    /// Tasks in the thread's local queues at the time of the snapshot.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queued
    }

    fn merge(self, other: Self) -> Self {
        Self {
            executed: self.executed.saturating_add(other.executed),
            stolen: self.stolen.saturating_add(other.stolen),
            parks: self.parks.saturating_add(other.parks),
            idle: self.idle.saturating_add(other.idle),
            parked: self.parked.saturating_add(other.parked),
            queued: self.queued.saturating_add(other.queued),
        }
    }
}

/// A pool metrics snapshot ([`ThreadPool::stats`](crate::ThreadPool::stats)).
///
/// Counters are cumulative since the pool was created; the difference of two snapshots gives the
/// metrics for the interval. A snapshot reads atomics without locking and does not stop the workers,
/// so the values of different counters may belong to slightly different moments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStats {
    workers: Vec<ThreadStats>,
    external: ThreadStats,
    global: [usize; 3],
}

impl PoolStats {
    pub(crate) fn collect(
        counters: &[CachePadded<Counters>],
        workers: usize,
        queued: impl Fn(usize) -> usize,
        global: [usize; 3],
    ) -> Self {
        let mut threads = counters
            .iter()
            .enumerate()
            .map(|(i, c)| c.snapshot(queued(i)));
        let worker_stats: Vec<ThreadStats> = threads.by_ref().take(workers).collect();
        let external = threads.fold(ThreadStats::default(), ThreadStats::merge);
        Self {
            workers: worker_stats,
            external,
            global,
        }
    }

    /// Worker metrics by index (`<prefix>-<index>`).
    #[must_use]
    pub fn workers(&self) -> &[ThreadStats] {
        &self.workers
    }

    /// Totals over threads outside the pool (the main thread in `scope`/`join`/`JobHandle::join`).
    #[must_use]
    pub fn external(&self) -> ThreadStats {
        self.external
    }

    /// Total tasks taken for execution by all threads.
    #[must_use]
    pub fn executed(&self) -> u64 {
        self.total().executed
    }

    /// Total steals.
    #[must_use]
    pub fn stolen(&self) -> u64 {
        self.total().stolen
    }

    /// Tasks in the global queue of priority `priority`.
    #[must_use]
    pub fn global_queued(&self, priority: Priority) -> usize {
        let index = match priority {
            Priority::Frame => 0,
            Priority::Normal => 1,
            Priority::Background => 2,
        };
        self.global.get(index).copied().unwrap_or(0)
    }

    /// Tasks in all queues (global and local) at the time of the snapshot.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.global
            .iter()
            .fold(self.total().queued, |acc, &n| acc.saturating_add(n))
    }

    fn total(&self) -> ThreadStats {
        self.workers
            .iter()
            .copied()
            .fold(self.external, ThreadStats::merge)
    }
}
