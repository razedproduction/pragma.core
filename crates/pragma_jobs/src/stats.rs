use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pragma_sync::__private::CachePadded;

use crate::Priority;

/// Счётчики одного потока пула. Каждый набор — на своей кэш-линии, а пишет его один поток
/// (воркер или владелец внешнего слота), поэтому инкремент — `load` + `store` без RMW
/// (`Relaxed`: метрики ничего не упорядочивают и на результат задач не влияют).
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

    /// Для набора, в который пишут несколько потоков (внешние потоки без слота).
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

/// Счётчики всех потоков пула: `0..workers` — воркеры, затем внешние слоты, последний —
/// внешние потоки без слота.
pub(crate) type CounterSet = Box<[CachePadded<Counters>]>;

pub(crate) fn counter_set(threads: usize) -> CounterSet {
    (0..threads.saturating_add(1))
        .map(|_| CachePadded::new(Counters::default()))
        .collect()
}

/// Снимок метрик одного потока (или группы внешних потоков), см. [`PoolStats`].
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
    /// Сколько задач поток взял на исполнение (свои, из глобальных очередей и украденные).
    #[must_use]
    pub fn executed(&self) -> u64 {
        self.executed
    }

    /// Сколько задач украдено из локальных очередей других потоков.
    #[must_use]
    pub fn stolen(&self) -> u64 {
        self.stolen
    }

    /// Сколько раз воркер засыпал (парковался) без работы.
    #[must_use]
    pub fn parks(&self) -> u64 {
        self.parks
    }

    /// Время без работы: поиск (spin) и сон. Только у воркеров.
    #[must_use]
    pub fn idle(&self) -> Duration {
        self.idle
    }

    /// Часть [`idle`](Self::idle), проведённая в парковке.
    #[must_use]
    pub fn parked(&self) -> Duration {
        self.parked
    }

    /// Задач в локальных очередях потока на момент снимка.
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

/// Снимок метрик пула ([`ThreadPool::stats`](crate::ThreadPool::stats)).
///
/// Счётчики накопительные с момента создания пула; разность двух снимков — метрики за
/// интервал. Снимок читает атомики без блокировок и воркеров не останавливает, поэтому
/// значения разных счётчиков могут относиться к немного разным моментам.
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

    /// Метрики воркеров по индексу (`<prefix>-<index>`).
    #[must_use]
    pub fn workers(&self) -> &[ThreadStats] {
        &self.workers
    }

    /// Суммарно по потокам вне пула (главный поток в `scope`/`join`/`JobHandle::join`).
    #[must_use]
    pub fn external(&self) -> ThreadStats {
        self.external
    }

    /// Всего задач, взятых на исполнение всеми потоками.
    #[must_use]
    pub fn executed(&self) -> u64 {
        self.total().executed
    }

    /// Всего краж.
    #[must_use]
    pub fn stolen(&self) -> u64 {
        self.total().stolen
    }

    /// Задач в глобальной очереди приоритета `priority`.
    #[must_use]
    pub fn global_queued(&self, priority: Priority) -> usize {
        let index = match priority {
            Priority::Frame => 0,
            Priority::Normal => 1,
            Priority::Background => 2,
        };
        self.global.get(index).copied().unwrap_or(0)
    }

    /// Задач во всех очередях (глобальных и локальных) на момент снимка.
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
