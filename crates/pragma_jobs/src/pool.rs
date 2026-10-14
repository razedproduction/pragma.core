use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_deque::{Injector, Steal, Stealer, Worker as Deque};
use pragma_sync::__private::{
    Backoff, CachePadded, JoinLatch, Parker, ScopedTask, StackJob, Unparker, join_on_stack,
};

use crate::stats::{CounterSet, Counters, PoolStats, counter_set};
#[cfg(debug_assertions)]
use crate::watchdog::{Active, Watchdog};
use crate::{JobsError, PoolConfig, Priority, ShutdownPolicy, sys};

/// A safety net against wakeup protocol bugs: a sleeping worker checks the queues anyway.
const PARK_TIMEOUT: Duration = Duration::from_millis(50);

/// A unit of work in the pool queues.
pub(crate) enum Job {
    /// A `'static` task (`spawn`); it catches its own panic and reports the result itself.
    Boxed(Box<dyn FnOnce() + Send + 'static>),
    /// A scope task (`scope`/`join`/`parallel_*`) with its lifetime erased in `pragma_sync`.
    Scoped(ScopedTask),
    /// The second branch of `join` on the owner's stack (DECISIONS #30).
    Stack(StackJob),
}

impl Job {
    /// Runs the task; returns the index of the pool participant that has to be woken
    /// (the owner of a `join` that fell asleep waiting for this task).
    fn run(self) -> Option<usize> {
        match self {
            Self::Boxed(f) => f(),
            Self::Scoped(task) => task.run(),
            Self::Stack(job) => return job.run(),
        }
        None
    }
}

/// How long the owner of a `join` waiting for a stolen branch sleeps before looking for work again
/// (the thief wakes it earlier when it finishes).
const JOIN_RECHECK: Duration = Duration::from_micros(500);

/// A worker's local queue: only `Frame` and `Normal` (background tasks are global only).
#[derive(Clone, Copy)]
enum Lane {
    Frame,
    Normal,
}

impl Lane {
    const ALL: [Self; 2] = [Self::Frame, Self::Normal];

    fn priority(self) -> Priority {
        match self {
            Self::Frame => Priority::Frame,
            Self::Normal => Priority::Normal,
        }
    }
}

/// The local queues of a pool participant and its parker: they move together between an external
/// slot and the thread that occupies it.
struct LocalQueues {
    frame: Deque<Job>,
    normal: Deque<Job>,
    parker: Parker,
}

impl LocalQueues {
    fn new() -> Self {
        Self {
            frame: Deque::new_lifo(),
            normal: Deque::new_lifo(),
            parker: Parker::new(),
        }
    }

    fn get(&self, lane: Lane) -> &Deque<Job> {
        match lane {
            Lane::Frame => &self.frame,
            Lane::Normal => &self.normal,
        }
    }

    fn drain(&self) {
        for lane in Lane::ALL {
            while let Some(job) = self.get(lane).pop() {
                drop_job(job);
            }
        }
    }

    fn stealers(&self) -> LocalStealers {
        LocalStealers {
            frame: self.frame.stealer(),
            normal: self.normal.stealer(),
        }
    }
}

struct LocalStealers {
    frame: Stealer<Job>,
    normal: Stealer<Job>,
}

impl LocalStealers {
    fn get(&self, lane: Lane) -> &Stealer<Job> {
        match lane {
            Lane::Frame => &self.frame,
            Lane::Normal => &self.normal,
        }
    }

    fn is_empty(&self) -> bool {
        self.frame.is_empty() && self.normal.is_empty()
    }
}

/// Worker startup: how many have not finished thread initialization yet (see [`ThreadPool::new`]).
struct Startup {
    pending: Mutex<usize>,
    done: Condvar,
}

impl Startup {
    fn new(workers: usize) -> Self {
        Self {
            pending: Mutex::new(workers),
            done: Condvar::new(),
        }
    }

    fn worker_ready(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        *pending = pending.saturating_sub(1);
        if *pending == 0 {
            self.done.notify_all();
        }
    }

    /// Waits until every worker has checked in via [`worker_ready`](Self::worker_ready).
    fn wait(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        while *pending > 0 {
            pending = self
                .done
                .wait(pending)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Pool state shared by the owner, the workers and the handles.
pub(crate) struct Shared {
    frame: Injector<Job>,
    normal: Injector<Job>,
    background: Injector<Job>,
    /// Workers first (`0..workers`), then external slots (`workers..`).
    stealers: Vec<LocalStealers>,
    /// One per worker; the index matches the index in `stealers`.
    unparkers: Vec<Unparker>,
    /// Wakers of all participants (workers, then external slots), for the owner of a `join`
    /// that fell asleep waiting for a stolen branch.
    participants: Vec<Unparker>,
    /// Local queues for external threads (see [`enter`]). `None` means the slot is taken.
    external_slots: Vec<Mutex<Option<LocalQueues>>>,
    /// Indices of sleeping workers (a stack: we wake the last one to fall asleep, its cache is warmer).
    idle: Mutex<Vec<usize>>,
    // Hot counters, each on its own cache line so they do not interfere with each other.
    sleeping: CachePadded<AtomicUsize>,
    /// Workers that are awake and looking for work (the spin phase): while there are any, a new
    /// task is picked up without waking sleepers.
    searching: CachePadded<AtomicUsize>,
    background_active: CachePadded<AtomicUsize>,
    shutdown: AtomicBool,
    /// Pool threads have exited and been `join`ed: nobody will run queued tasks anymore.
    stopped: AtomicBool,
    policy: ShutdownPolicy,
    background_limit: usize,
    /// `yield` rounds in the search phase before parking (`PoolConfig::with_idle_spin`).
    idle_spin: u32,
    /// Metrics (see [`PoolStats`]): one set per `stealers` index, and the last one for
    /// external threads without a slot.
    counters: CounterSet,
    startup: Startup,
    #[cfg(debug_assertions)]
    watchdog: Option<Watchdog>,
}

thread_local! {
    /// The worker context, if the current thread is a worker of some pool.
    static WORKER: Cell<Option<Rc<WorkerCtx>>> = const { Cell::new(None) };
    /// Priority of the task running right now (`None`: the thread is outside a pool task).
    static CURRENT_PRIORITY: Cell<Option<Priority>> = const { Cell::new(None) };
}

fn current_worker() -> Option<Rc<WorkerCtx>> {
    WORKER
        .try_with(|cell| {
            let ctx = cell.take();
            let copy = ctx.clone();
            cell.set(ctx);
            copy
        })
        .ok()
        .flatten()
}

fn current_worker_of(shared: &Arc<Shared>) -> Option<Rc<WorkerCtx>> {
    current_worker().filter(|w| Arc::ptr_eq(&w.shared, shared))
}

/// Number of external slots: this many threads outside the pool can wait "like workers" at once.
const EXTERNAL_SLOTS: usize = 4;

/// Runs `f` with the current thread made a temporary participant of the pool.
///
/// An external thread (for example the main one) takes a free external slot: its own
/// LIFO queue, visible to thieves. Subtasks of `join`/`scope` go into it, and the wait takes
/// its own fresh tasks first, like a worker. Without a slot, subtasks would go into the FIFO injector,
/// and helping while waiting would take the oldest (largest) tasks, growing the stack at every level.
/// If all slots are taken, the thread works through the global queues (slower, but correct).
pub(crate) fn enter<R>(shared: &Arc<Shared>, f: impl FnOnce() -> R) -> R {
    if current_worker_of(shared).is_some() {
        return f();
    }
    let Some((slot, local)) = shared.acquire_external_slot() else {
        return f();
    };
    let index = shared.unparkers.len().saturating_add(slot);
    let (ctx, placeholder) = external_ctx(shared, index, local);
    let previous = WORKER
        .try_with(|cell| cell.replace(Some(ctx)))
        .ok()
        .flatten();
    let _guard = ExternalGuard {
        shared,
        slot,
        previous,
        placeholder,
    };
    f()
}

thread_local! {
    /// The external slot context from this thread's previous [`enter`], with placeholder queues
    /// instead of the real ones. Without it, every entry of the main thread into the pool (once per
    /// frame) would cost an `Rc` allocation (DECISIONS #18). It holds the last pool's `Arc<Shared>`
    /// until the next `enter` or thread exit: this does not hold back the pool threads, only memory.
    static SPARE_EXTERNAL: Cell<Option<Rc<WorkerCtx>>> = const { Cell::new(None) };
}

/// The external thread's context with the slot queues `local`, and the placeholder it will put back
/// in their place on exit (`None`: the context is new, and [`ExternalGuard`] creates the placeholder).
fn external_ctx(
    shared: &Arc<Shared>,
    index: usize,
    local: LocalQueues,
) -> (Rc<WorkerCtx>, Option<LocalQueues>) {
    if let Some(mut spare) = SPARE_EXTERNAL.try_with(Cell::take).ok().flatten()
        && let Some(ctx) = Rc::get_mut(&mut spare)
    {
        let placeholder = ctx.reuse(shared, index, local);
        return (spare, Some(placeholder));
    }
    let ctx = Rc::new(WorkerCtx::new(Arc::clone(shared), index, local, true));
    (ctx, None)
}

/// Returns the external slot and restores the thread's previous context (also on panic).
struct ExternalGuard<'a> {
    shared: &'a Shared,
    slot: usize,
    previous: Option<Rc<WorkerCtx>>,
    placeholder: Option<LocalQueues>,
}

impl Drop for ExternalGuard<'_> {
    fn drop(&mut self) {
        let ctx = WORKER
            .try_with(|cell| cell.replace(self.previous.take()))
            .ok()
            .flatten();
        let Some(slot) = self.shared.external_slots.get(self.slot) else {
            return;
        };
        let Some(mut ctx) = ctx else {
            tracing::error!("external pool slot context was not released; slot is lost");
            return;
        };
        let Some(inner) = Rc::get_mut(&mut ctx) else {
            tracing::error!("external pool slot context is still shared; slot is lost");
            return;
        };
        // Tasks left in the queue (for example a `spawn` from a `scope` body) are not lost:
        // the queue goes back into the slot, where thieves still see it. A placeholder takes
        // its place, and the context is reused by the next `enter`.
        let placeholder = self.placeholder.take().unwrap_or_else(LocalQueues::new);
        let local = std::mem::replace(&mut inner.local, placeholder);
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(local);
        let _ = SPARE_EXTERNAL.try_with(|spare| spare.set(Some(ctx)));
    }
}

/// The priority for `scope`/`join`/`parallel_*` subtasks: inherited from the current task.
/// Outside the pool it is `Frame` (frame work is launched by the main thread). Subtasks of a
/// background task go to `Normal`: otherwise they would hit the `Background` limit held by their own parent.
pub(crate) fn inherited_priority() -> Priority {
    match CURRENT_PRIORITY.try_with(Cell::get).ok().flatten() {
        None => Priority::Frame,
        Some(Priority::Background) => Priority::Normal,
        Some(other) => other,
    }
}

/// The priority of the pool task the current thread is running (`None`: the thread is outside pool tasks).
pub(crate) fn current_priority() -> Option<Priority> {
    CURRENT_PRIORITY.try_with(Cell::get).ok().flatten()
}

/// Debug builds: registers a labeled task in the watchdog cell of the current pool thread.
#[cfg(debug_assertions)]
pub(crate) struct WatchGuard {
    ctx: Rc<WorkerCtx>,
    previous: Option<Active>,
}

#[cfg(debug_assertions)]
pub(crate) fn watch(kind: &'static str, name: &str) -> Option<WatchGuard> {
    let ctx = current_worker()?;
    let previous = ctx.shared.watchdog.as_ref()?.enter(ctx.index, kind, name);
    Some(WatchGuard { ctx, previous })
}

#[cfg(debug_assertions)]
impl Drop for WatchGuard {
    fn drop(&mut self) {
        if let Some(watchdog) = self.ctx.shared.watchdog.as_ref() {
            watchdog.exit(self.ctx.index, self.previous.take());
        }
    }
}

/// Runs a task with the current priority set. A panic that escapes past the task wrapper
/// (for example from a captured value's destructor) does not kill the thread.
fn execute(shared: &Shared, job: Job, priority: Priority) {
    let previous = CURRENT_PRIORITY
        .try_with(|p| p.replace(Some(priority)))
        .ok()
        .flatten();
    match panic::catch_unwind(AssertUnwindSafe(|| job.run())) {
        Ok(Some(owner)) => shared.wake_participant(owner),
        Ok(None) => {}
        Err(_) => tracing::error!("panic escaped a job wrapper; worker keeps running"),
    }
    // An access error is only possible while TLS is torn down at thread exit; then there is nothing to restore.
    let _ = CURRENT_PRIORITY.try_with(|p| p.set(previous));
}

/// Drops a task that has not run. The destructor of captured values (for example the future
/// of a cancelled async task) is user code: its panic must not bring down the thread
/// that is stopping the pool or scheduling the task.
fn drop_job(job: Job) {
    if panic::catch_unwind(AssertUnwindSafe(|| drop(job))).is_err() {
        tracing::error!("panic while dropping a cancelled job");
    }
}

fn steal_loop<T>(mut attempt: impl FnMut() -> Steal<T>) -> Option<T> {
    loop {
        match attempt() {
            Steal::Success(job) => return Some(job),
            Steal::Empty => return None,
            Steal::Retry => {}
        }
    }
}

impl Shared {
    fn injector(&self, priority: Priority) -> &Injector<Job> {
        match priority {
            Priority::Frame => &self.frame,
            Priority::Normal => &self.normal,
            Priority::Background => &self.background,
        }
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Pushes a detached task if the pool still accepts work; otherwise drops it
    /// (see [`Spawner::spawn_detached`](crate::Spawner::spawn_detached)).
    pub(crate) fn push_detached(self: &Arc<Self>, job: Job, priority: Priority) -> bool {
        if self.is_shutdown() {
            drop_job(job);
            return false;
        }
        self.push(job, priority);
        // A race with the pool's `Drop` that has already `join`ed the threads: paired with the `fence`
        // before `drain_queues` in `Drop`; either it sees our task, or we see `stopped`.
        fence(Ordering::SeqCst);
        if self.stopped.load(Ordering::SeqCst) {
            self.drain_queues();
            return false;
        }
        true
    }

    /// Pushes a task into a queue and wakes one sleeping worker.
    pub(crate) fn push(self: &Arc<Self>, job: Job, priority: Priority) {
        let local = match priority {
            Priority::Background => None,
            Priority::Frame | Priority::Normal => current_worker_of(self),
        };
        match (local, priority) {
            (Some(worker), Priority::Frame) => worker.local.frame.push(job),
            (Some(worker), _) => worker.local.normal.push(job),
            (None, priority) => self.injector(priority).push(job),
        }
        self.notify_one();
    }

    fn notify_one(&self) {
        // Paired with the `fence` in `WorkerCtx::sleep`: either the worker sees the task when it
        // rechecks, or we see it in `sleeping`.
        fence(Ordering::SeqCst);
        // A searching worker will pick the task up itself (and if it falls asleep, it rechecks the
        // queues before parking), so no wakeup system call is needed.
        if self.sleeping.load(Ordering::SeqCst) == 0 || self.searching.load(Ordering::SeqCst) > 0 {
            return;
        }
        self.wake_one();
    }

    /// Like [`notify_one`](Self::notify_one), but without the fence: used after a push into the own
    /// queue of a participant that will wait for this task itself (`join`). Without the fence the
    /// check may miss a worker that is falling asleep, but that costs only parallelism, not a hang:
    /// the owner pops the task from its own queue itself, and the sleeper wakes on the next push
    /// or after `PARK_TIMEOUT` (DECISIONS #40). The fence is ~10% of the time of a nested `join`.
    fn notify_one_local(&self) {
        if self.sleeping.load(Ordering::Relaxed) == 0 || self.searching.load(Ordering::Relaxed) > 0
        {
            return;
        }
        self.wake_one();
    }

    fn wake_one(&self) {
        let target = {
            let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
            let target = idle.pop();
            self.sleeping.store(idle.len(), Ordering::SeqCst);
            target
        };
        if let Some(unparker) = target.and_then(|i| self.unparkers.get(i)) {
            unparker.unpark();
        }
    }

    /// Wakes pool participant `index` (the owner of a `join` asleep on its parker).
    fn wake_participant(&self, index: usize) {
        if let Some(unparker) = self.participants.get(index) {
            unparker.unpark();
        }
    }

    fn wake_all(&self) {
        {
            let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
            idle.clear();
            self.sleeping.store(0, Ordering::SeqCst);
        }
        for unparker in &self.unparkers {
            unparker.unpark();
        }
    }

    fn unregister_idle(&self, index: usize) {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        idle.retain(|&i| i != index);
        self.sleeping.store(idle.len(), Ordering::SeqCst);
    }

    fn has_visible_work(&self) -> bool {
        !self.frame.is_empty()
            || !self.normal.is_empty()
            || (!self.background.is_empty()
                && self.background_active.load(Ordering::SeqCst) < self.background_limit)
            || self.stealers.iter().any(|s| !s.is_empty())
    }

    fn try_acquire_background(&self) -> bool {
        let mut current = self.background_active.load(Ordering::Acquire);
        loop {
            if current >= self.background_limit {
                return false;
            }
            match self.background_active.compare_exchange_weak(
                current,
                current.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    fn release_background(&self) {
        self.background_active.fetch_sub(1, Ordering::AcqRel);
    }

    /// Finding work for a thread that is not a worker of this pool: only `Frame` and
    /// `Normal`. An external thread does not take background tasks; otherwise the main thread could
    /// get stuck in baking for a long time in the middle of a frame.
    fn find_external(&self) -> Option<(Job, Priority)> {
        for lane in Lane::ALL {
            let priority = lane.priority();
            if let Some(job) = steal_loop(|| self.injector(priority).steal()) {
                return Some((job, priority));
            }
            for stealers in &self.stealers {
                if let Some(job) = steal_loop(|| stealers.get(lane).steal()) {
                    if let Some(counters) = self.external_counters() {
                        counters.stolen_shared();
                    }
                    return Some((job, priority));
                }
            }
        }
        None
    }

    /// Metrics of external threads without a slot (several threads write them, so RMW only).
    fn external_counters(&self) -> Option<&Counters> {
        self.counters.last().map(|c| &**c)
    }

    fn stats(&self) -> PoolStats {
        let queued = |i: usize| {
            self.stealers
                .get(i)
                .map_or(0, |s| s.frame.len().saturating_add(s.normal.len()))
        };
        PoolStats::collect(
            &self.counters,
            self.unparkers.len(),
            queued,
            [self.frame.len(), self.normal.len(), self.background.len()],
        )
    }

    fn acquire_external_slot(&self) -> Option<(usize, LocalQueues)> {
        self.external_slots
            .iter()
            .enumerate()
            .find_map(|(i, slot)| {
                slot.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
                    .map(|local| (i, local))
            })
    }

    /// Drops the tasks of the global queues and of free external slots
    /// (their handles get `Cancelled`).
    fn drain_queues(&self) {
        for priority in [Priority::Frame, Priority::Normal, Priority::Background] {
            while let Some(job) = steal_loop(|| self.injector(priority).steal()) {
                drop_job(job);
            }
        }
        for slot in &self.external_slots {
            if let Some(local) = slot.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                local.drain();
            }
        }
    }

    /// Whether the caller's thread is a worker of this pool or an external thread inside `scope`/`join`.
    pub(crate) fn in_context(self: &Arc<Self>) -> bool {
        current_worker_of(self).is_some()
    }
}

/// `join` via a stack job if the current thread is a participant of pool `shared` (a worker or
/// an external thread in a temporary slot); `Err`: not a participant, `a` and `b` are returned untouched.
#[expect(
    clippy::type_complexity,
    reason = "internal result of the two branches"
)]
pub(crate) fn join_as_participant<A, B, RA, RB>(
    shared: &Arc<Shared>,
    a: A,
    b: B,
) -> Result<(std::thread::Result<RA>, Option<std::thread::Result<RB>>), (A, B)>
where
    A: FnOnce() -> RA,
    B: FnOnce() -> RB + Send,
    RB: Send,
{
    match current_worker_of(shared) {
        Some(ctx) => Ok(ctx.join(a, b)),
        None => Err((a, b)),
    }
}

/// Runs one unit of someone else's work, if there is any (help-while-wait).
pub(crate) fn help_once(shared: &Arc<Shared>) -> bool {
    if let Some(worker) = current_worker_of(shared) {
        return worker.run_one();
    }
    match shared.find_external() {
        Some((job, priority)) => {
            if let Some(counters) = shared.external_counters() {
                counters.executed_shared();
            }
            execute(shared, job, priority);
            true
        }
        None => false,
    }
}

struct Found {
    job: Job,
    priority: Priority,
    /// The task took a `Background` slot that must be released after it runs.
    background_slot: bool,
}

struct WorkerCtx {
    shared: Arc<Shared>,
    index: usize,
    local: LocalQueues,
    rng: Cell<u64>,
    /// The worker is already running a background task: nested background tasks (help-while-wait)
    /// count against the same slot; otherwise, with a limit of 1, waiting for a background task
    /// inside a background task would hang forever.
    holds_background: Cell<bool>,
    /// An external thread in a temporary slot (see [`enter`]): takes no background tasks and never sleeps.
    external: bool,
}

impl WorkerCtx {
    fn new(shared: Arc<Shared>, index: usize, local: LocalQueues, external: bool) -> Self {
        Self {
            shared,
            index,
            local,
            rng: Cell::new(Self::seed(index)),
            holds_background: Cell::new(false),
            external,
        }
    }

    fn seed(index: usize) -> u64 {
        u64::try_from(index)
            .unwrap_or(0)
            .wrapping_add(1)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            | 1
    }

    /// Re-targets the cached external context to slot `index` of pool `shared`, as if
    /// new; returns the placeholder queues that stood in place of `local`.
    fn reuse(&mut self, shared: &Arc<Shared>, index: usize, local: LocalQueues) -> LocalQueues {
        if !Arc::ptr_eq(&self.shared, shared) {
            self.shared = Arc::clone(shared);
        }
        self.index = index;
        self.rng.set(Self::seed(index));
        self.holds_background.set(false);
        self.external = true;
        std::mem::replace(&mut self.local, local)
    }

    /// This thread's metrics: only it writes them (a worker or the owner of an external slot).
    fn counters(&self) -> Option<&Counters> {
        self.shared.counters.get(self.index).map(|c| &**c)
    }

    /// xorshift64: deterministic victim selection without the system RNG.
    fn next_random(&self) -> u64 {
        let mut x = self.rng.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.set(x);
        x
    }

    /// Search order: for `Frame`, then for `Normal`: own queue -> global queue ->
    /// steal from a random victim; then `Background` within the limit.
    fn find(&self) -> Option<Found> {
        for lane in Lane::ALL {
            if let Some(job) = self.find_in_lane(lane) {
                return Some(Found {
                    job,
                    priority: lane.priority(),
                    background_slot: false,
                });
            }
        }
        self.find_background()
    }

    fn find_in_lane(&self, lane: Lane) -> Option<Job> {
        let local = self.local.get(lane);
        if let Some(job) = local.pop() {
            return Some(job);
        }
        let injector = self.shared.injector(lane.priority());
        if let Some(job) = steal_loop(|| injector.steal_batch_and_pop(local)) {
            return Some(job);
        }
        let victims = self.shared.stealers.len();
        let start = usize::try_from(self.next_random())
            .unwrap_or(0)
            .checked_rem(victims)
            .unwrap_or(0);
        loop {
            let mut retry = false;
            for (i, stealers) in self
                .shared
                .stealers
                .iter()
                .enumerate()
                .cycle()
                .skip(start)
                .take(victims)
            {
                // crossbeam's `steal` first enters an epoch (`epoch::pin`), and almost all victims
                // on a search round are empty: a cheap length check without entering the epoch.
                if i == self.index || stealers.get(lane).is_empty() {
                    continue;
                }
                // Steal one task at a time from a victim (the oldest, usually the largest):
                // stealing a batch also carries off small `join` branches that the owner is
                // about to finish waiting for itself, and a local pop turns into ping-pong between threads.
                match stealers.get(lane).steal() {
                    Steal::Success(job) => {
                        if let Some(counters) = self.counters() {
                            counters.stolen();
                        }
                        return Some(job);
                    }
                    Steal::Retry => retry = true,
                    Steal::Empty => {}
                }
            }
            if !retry {
                return None;
            }
        }
    }

    fn find_background(&self) -> Option<Found> {
        // An external thread (the main one) does not take background work: getting stuck in baking
        // in the middle of a frame is worse than waiting.
        if self.external || self.shared.background.is_empty() {
            return None;
        }
        let acquired = if self.holds_background.get() {
            false
        } else if self.shared.try_acquire_background() {
            true
        } else {
            return None;
        };
        match steal_loop(|| self.shared.background.steal()) {
            Some(job) => Some(Found {
                job,
                priority: Priority::Background,
                background_slot: acquired,
            }),
            None => {
                if acquired {
                    self.shared.release_background();
                }
                None
            }
        }
    }

    fn run_one(&self) -> bool {
        let Some(found) = self.find() else {
            return false;
        };
        self.run_found(found);
        true
    }

    fn run_found(&self, found: Found) {
        if found.background_slot {
            self.holds_background.set(true);
        }
        // Before running: by the time a waiter sees the task completed, the counter has already
        // been incremented (a snapshot after `join` matches the number of tasks).
        if let Some(counters) = self.counters() {
            counters.executed();
        }
        execute(&self.shared, found.job, found.priority);
        if found.background_slot {
            self.holds_background.set(false);
            self.shared.release_background();
        }
    }

    /// The spin phase: the worker stays awake and looks for work, registered in `searching`.
    fn search(&self) -> Option<Found> {
        self.shared.searching.fetch_add(1, Ordering::SeqCst);
        let backoff = Backoff::new();
        let mut yields = 0_u32;
        let mut found = None;
        loop {
            // First a short spin (`Backoff`), then `idle_spin` rounds of `yield`: threads
            // that are about to get work (the next wave of `join`, the next frame) do not
            // pay for parking and an OS wakeup (see `docs/perf.md`).
            if !backoff.is_completed() {
                backoff.snooze();
            } else if yields < self.shared.idle_spin {
                thread::yield_now();
                yields = yields.saturating_add(1);
            } else {
                break;
            }
            found = self.find();
            if found.is_some() {
                break;
            }
        }
        self.shared.searching.fetch_sub(1, Ordering::SeqCst);
        // While we were searching, pushers did not wake sleepers; if there is more than one task,
        // pass the baton to the next worker.
        // The cheap `sleeping` check first: scanning other queues when there is nobody to wake
        // means needless cache misses on the hot path.
        if found.is_some()
            && self.shared.sleeping.load(Ordering::SeqCst) > 0
            && self.shared.has_visible_work()
        {
            self.shared.notify_one();
        }
        found
    }

    /// `join` on a pool participant (a worker or an external slot): `b` is a stack job
    /// (DECISIONS #30). If `b` was not stolen, the thread takes it back from its queue and runs it
    /// itself, with no atomic RMW and no allocations; a stolen one it waits for while helping, then
    /// sleeps until the thief wakes it.
    pub(crate) fn join<A, B, RA, RB>(
        &self,
        a: A,
        b: B,
    ) -> (std::thread::Result<RA>, Option<std::thread::Result<RB>>)
    where
        A: FnOnce() -> RA,
        B: FnOnce() -> RB + Send,
        RB: Send,
    {
        let lane = match inherited_priority() {
            Priority::Frame => Lane::Frame,
            Priority::Normal | Priority::Background => Lane::Normal,
        };
        let queue = self.local.get(lane);
        let backoff = Backoff::new();
        let (ra, rb) = join_on_stack(
            self.index,
            b,
            |job| {
                let id = job.id();
                queue.push(Job::Stack(job));
                self.shared.notify_one_local();
                let ra = panic::catch_unwind(AssertUnwindSafe(a));
                // `a` has taken back everything it pushed itself (except a `spawn` it does not wait for),
                // so our `b` is on top, unless it was stolen.
                match queue.pop() {
                    Some(Job::Stack(job)) if job.id() == id => {
                        if let Some(counters) = self.counters() {
                            counters.executed();
                        }
                        job.run_inline();
                    }
                    Some(other) => self.run_found(Found {
                        job: other,
                        priority: lane.priority(),
                        background_slot: false,
                    }),
                    None => {}
                }
                ra
            },
            |latch| self.wait_step(latch, &backoff),
        );
        (ra, rb)
    }

    /// One step of waiting for a stolen `join` branch: help, spin or sleep.
    fn wait_step(&self, latch: &JoinLatch<'_>, backoff: &Backoff) {
        if self.run_one() {
            backoff.reset();
        } else if !backoff.is_completed() {
            backoff.snooze();
        } else if latch.prepare_park() {
            // The thief wakes us by index (`Shared::wake_participant`); the timeout lets us look again
            // for work that appeared while we slept (new tasks wake only workers).
            self.local.parker.park_timeout(JOIN_RECHECK);
        }
    }

    fn sleep(&self) {
        let parker = &self.local.parker;
        {
            let mut idle = self
                .shared
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            idle.push(self.index);
            self.shared.sleeping.store(idle.len(), Ordering::SeqCst);
        }
        // Paired with the `fence` in `Shared::notify_one`.
        fence(Ordering::SeqCst);
        if !self.shared.has_visible_work() && !self.shared.is_shutdown() {
            let parked = Instant::now();
            parker.park_timeout(PARK_TIMEOUT);
            if let Some(counters) = self.counters() {
                counters.parked(parked.elapsed());
            }
        }
        self.shared.unregister_idle(self.index);
    }
}

/// The worker's platform settings (`sys`), applied by the worker itself at startup.
#[derive(Clone, Copy)]
struct ThreadSetup {
    priority: Option<sys::ThreadPriority>,
    core: Option<usize>,
}

impl ThreadSetup {
    /// An OS refusal is no reason not to start the worker: a warning, and it runs without the setting.
    fn apply(self, index: usize) {
        if let Some(priority) = self.priority
            && let Err(error) = sys::set_current_thread_priority(priority)
        {
            tracing::warn!(worker = index, ?priority, %error, "failed to set worker thread priority");
        }
        if let Some(core) = self.core
            && let Err(error) = sys::pin_current_thread(core)
        {
            tracing::warn!(worker = index, core, %error, "failed to pin worker thread to a core");
        }
    }
}

fn worker_main(shared: Arc<Shared>, index: usize, local: LocalQueues, setup: ThreadSetup) {
    setup.apply(index);
    // The first steal registers the thread in `crossbeam-epoch` (a handle allocation in TLS). Do it
    // right away rather than in the middle of a frame: with the `is_empty` check before stealing, a
    // worker may not reach `steal` for a long time (allocation-free graph frame, `tests/alloc.rs`).
    if let Some(own) = shared.stealers.get(index) {
        let _ = own.frame.steal();
    }
    let ctx = Rc::new(WorkerCtx::new(shared, index, local, false));
    let _ = WORKER.try_with(|cell| cell.set(Some(Rc::clone(&ctx))));
    // Everything the thread allocates once (the epoch handle, the context, the TLS destructor list)
    // has been allocated: `ThreadPool::new` may return (DECISIONS #39).
    ctx.shared.startup.worker_ready();
    loop {
        if ctx.shared.policy == ShutdownPolicy::Cancel && ctx.shared.is_shutdown() {
            break;
        }
        if ctx.run_one() {
            continue;
        }
        let idle = Instant::now();
        let found = ctx.search();
        let stop = found.is_none() && ctx.shared.is_shutdown();
        if found.is_none() && !stop {
            ctx.sleep();
        }
        if let Some(counters) = ctx.counters() {
            counters.idle(idle.elapsed());
        }
        match found {
            Some(found) => ctx.run_found(found),
            None if stop => break,
            None => {}
        }
    }
    let _ = WORKER.try_with(Cell::take);
    // `Cancel` policy: tasks left in the own queue are dropped (handles get `Cancelled`).
    ctx.local.drain();
}

/// A work-stealing pool of worker threads.
///
/// Created with [`ThreadPool::new`]; `Drop` stops and `join`s all threads
/// (see [`ShutdownPolicy`]). A panic in a task does not kill the worker and is returned as
/// [`JobError::Panicked`](crate::JobError::Panicked).
///
/// An engine usually creates one pool for the lifetime of the process and passes `&ThreadPool`
/// to its systems. The thread that calls `scope`/`join`/`parallel_*` (typically the main thread)
/// takes part in the work while it waits, which is why the default config has one worker fewer
/// than there are cores.
///
/// ```
/// use pragma_jobs::{PoolConfig, ThreadPool};
///
/// let pool = ThreadPool::new(PoolConfig::default().with_name_prefix("engine-worker"))?;
/// assert!(pool.workers() >= 1);
///
/// let (left, right) = pool.join(|| (0..1000_u64).sum::<u64>(), || (1000..2000_u64).sum::<u64>())?;
/// assert_eq!(left + right, (0..2000).sum());
///
/// drop(pool); // drains the queues and joins every worker thread
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct ThreadPool {
    pub(crate) shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    /// The watchdog thread (debug builds only, and only if enabled in the config).
    #[cfg(debug_assertions)]
    watchdog: Option<JoinHandle<()>>,
}

/// The watchdog thread: scans the pool threads' cells until the pool begins shutting down.
#[cfg(debug_assertions)]
fn watchdog_main(shared: &Shared) {
    let Some(watchdog) = shared.watchdog.as_ref() else {
        return;
    };
    while !shared.is_shutdown() {
        thread::park_timeout(watchdog.period());
        watchdog.scan();
    }
}

impl std::fmt::Debug for ThreadPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadPool")
            .field("workers", &self.threads.len())
            .field("background_limit", &self.shared.background_limit)
            .field("shutdown", &self.shared.policy)
            .finish()
    }
}

impl ThreadPool {
    /// Starts the pool. Threads are named `<name_prefix>-<index>`.
    ///
    /// Returns once every worker has finished thread initialization (priority and affinity
    /// applied, one-time thread allocations done): the first frames do not pay for starting
    /// threads, even if there are not enough cores for the workers.
    ///
    /// If the OS refuses to create a thread, the threads already started are stopped and `join`ed,
    /// and [`JobsError::SpawnFailed`] is returned.
    ///
    /// ```
    /// use pragma_jobs::{JobsError, PoolConfig, ThreadPool};
    ///
    /// let pool = ThreadPool::new(PoolConfig::default().with_workers(3))?;
    /// assert_eq!(pool.workers(), 3);
    ///
    /// // An invalid config is rejected before any thread is started.
    /// let err = ThreadPool::new(PoolConfig::default().with_workers(0)).unwrap_err();
    /// assert!(matches!(err, JobsError::InvalidConfig { .. }));
    /// # Ok::<(), JobsError>(())
    /// ```
    pub fn new(config: PoolConfig) -> Result<Self, JobsError> {
        let config = config.validate()?;
        let workers = config.workers();
        let mut locals = Vec::with_capacity(workers);
        let mut stealers = Vec::with_capacity(workers);
        let mut unparkers = Vec::with_capacity(workers);
        for _ in 0..workers {
            let local = LocalQueues::new();
            stealers.push(local.stealers());
            unparkers.push(local.parker.unparker());
            locals.push(local);
        }
        let mut participants = unparkers.clone();
        let external_slots = (0..EXTERNAL_SLOTS)
            .map(|_| {
                let local = LocalQueues::new();
                stealers.push(local.stealers());
                participants.push(local.parker.unparker());
                Mutex::new(Some(local))
            })
            .collect();
        let counters = counter_set(stealers.len());
        #[cfg(debug_assertions)]
        let watchdog = config.watchdog().map(|threshold| {
            let names = (0..workers)
                .map(|i| format!("{}-{i}", config.name_prefix()))
                .chain((0..EXTERNAL_SLOTS).map(|i| format!("external-{i}")))
                .collect();
            Watchdog::new(threshold, names)
        });
        let shared = Arc::new(Shared {
            frame: Injector::new(),
            normal: Injector::new(),
            background: Injector::new(),
            stealers,
            unparkers,
            participants,
            external_slots,
            idle: Mutex::new(Vec::with_capacity(workers)),
            sleeping: CachePadded::new(AtomicUsize::new(0)),
            searching: CachePadded::new(AtomicUsize::new(0)),
            background_active: CachePadded::new(AtomicUsize::new(0)),
            shutdown: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            policy: config.shutdown(),
            background_limit: config.background_limit(),
            idle_spin: config.idle_spin(),
            counters,
            startup: Startup::new(workers),
            #[cfg(debug_assertions)]
            watchdog,
        });
        let mut pool = Self {
            shared,
            threads: Vec::with_capacity(workers),
            #[cfg(debug_assertions)]
            watchdog: None,
        };
        for (index, local) in locals.into_iter().enumerate() {
            let name = format!("{}-{index}", config.name_prefix());
            let mut builder = thread::Builder::new().name(name.clone());
            if let Some(bytes) = config.stack_size() {
                builder = builder.stack_size(bytes);
            }
            let setup = ThreadSetup {
                priority: config.thread_priority(),
                core: config.core_for(index),
            };
            let shared = Arc::clone(&pool.shared);
            match builder.spawn(move || worker_main(shared, index, local, setup)) {
                Ok(handle) => pool.threads.push(handle),
                // `pool` is dropped here and stops the threads already started.
                Err(source) => return Err(JobsError::SpawnFailed { name, source }),
            }
        }
        // Otherwise the one-time initialization of a worker that got no core under
        // oversubscription would happen in the middle of the first frames (DECISIONS #39).
        pool.shared.startup.wait();
        #[cfg(debug_assertions)]
        pool.start_watchdog(config.name_prefix());
        Ok(pool)
    }

    /// Debug builds: starts the watchdog thread. If that fails, the pool runs without it.
    #[cfg(debug_assertions)]
    fn start_watchdog(&mut self, prefix: &str) {
        if self.shared.watchdog.is_none() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        match thread::Builder::new()
            .name(format!("{prefix}-watchdog"))
            .spawn(move || watchdog_main(&shared))
        {
            Ok(handle) => self.watchdog = Some(handle),
            Err(error) => tracing::warn!(%error, "failed to spawn pool watchdog thread"),
        }
    }

    /// A snapshot of pool metrics: tasks, steals, idle time and parks per worker, queue depths.
    ///
    /// Does not block the workers (it only reads atomics and queue lengths) and does not affect
    /// task execution; only the snapshot itself is allocated.
    ///
    /// ```
    /// use pragma_jobs::{PoolConfig, Priority, ThreadPool};
    ///
    /// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
    /// let before = pool.stats();
    /// for _ in 0..10 {
    ///     pool.spawn(Priority::Normal, || ()).join()?;
    /// }
    /// let after = pool.stats();
    /// assert_eq!(after.executed() - before.executed(), 10);
    /// assert_eq!(after.workers().len(), 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn stats(&self) -> PoolStats {
        self.shared.stats()
    }

    /// The index of this pool's worker the call runs on (`None`: the thread is not a worker
    /// of this pool, for example the main thread inside `scope`).
    ///
    /// Useful for per-worker scratch buffers: index an array of `workers()` buffers with it.
    ///
    /// ```
    /// use pragma_jobs::{PoolConfig, Priority, ThreadPool};
    ///
    /// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
    /// assert_eq!(pool.current_worker_index(), None); // the main thread
    /// assert!(!pool.is_worker_thread());
    ///
    /// let pool = std::sync::Arc::new(pool);
    /// let inner = std::sync::Arc::clone(&pool);
    /// let index = pool.spawn(Priority::Normal, move || inner.current_worker_index());
    /// // `join` may run the task on the waiting thread itself (help-while-wait), so either is fine:
    /// assert!(matches!(index.join()?, None | Some(0..=1)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn current_worker_index(&self) -> Option<usize> {
        current_worker_of(&self.shared)
            .filter(|w| !w.external)
            .map(|w| w.index)
    }

    /// Number of worker threads.
    #[must_use]
    pub fn workers(&self) -> usize {
        self.threads.len()
    }

    /// Whether the current thread is a worker of this pool.
    #[must_use]
    pub fn is_worker_thread(&self) -> bool {
        current_worker_of(&self.shared).is_some_and(|w| !w.external)
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.wake_all();
        #[cfg(debug_assertions)]
        if let Some(watchdog) = self.watchdog.take() {
            watchdog.thread().unpark();
            if watchdog.join().is_err() {
                tracing::error!("pool watchdog thread terminated with a panic");
            }
        }
        let me = thread::current().id();
        let mut joined_all = true;
        for handle in self.threads.drain(..) {
            if handle.thread().id() == me {
                // The pool is dropped from inside its own task: do not join ourselves, the thread
                // will exit by itself after the current task.
                joined_all = false;
                continue;
            }
            if handle.join().is_err() {
                tracing::error!("pragma worker thread terminated with a panic");
            }
        }
        if joined_all {
            self.shared.stopped.store(true, Ordering::SeqCst);
            // Paired with the `fence` in `Shared::push_detached`.
            fence(Ordering::SeqCst);
        }
        if joined_all || self.shared.policy == ShutdownPolicy::Cancel {
            self.shared.drain_queues();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Workers that got no core (more workers than cores) are already initialized by the time
    /// `new` returns: their one-time allocations do not land in the first frames.
    #[test]
    fn new_returns_after_every_worker_is_initialized() {
        let workers = thread::available_parallelism().map_or(4, |n| n.get().saturating_mul(2));
        for _ in 0..20 {
            let pool = ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap();
            assert_eq!(pool.shared.startup.pending(), 0);
        }
    }
}
