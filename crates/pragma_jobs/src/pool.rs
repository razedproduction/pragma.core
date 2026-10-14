use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_deque::{Injector, Steal, Stealer, Worker as Deque};
use pragma_base::tracing;
use pragma_sync::{
    Backoff, CachePadded, JoinLatch, Parker, ScopedTask, StackJob, Unparker, join_on_stack,
};

use crate::stats::{CounterSet, Counters, PoolStats, counter_set};
#[cfg(debug_assertions)]
use crate::watchdog::{Active, Watchdog};
use crate::{JobsError, PoolConfig, Priority, ShutdownPolicy, sys};

/// Страховка от ошибок протокола пробуждения: спящий воркер всё равно проверит очереди.
const PARK_TIMEOUT: Duration = Duration::from_millis(50);

/// Единица работы в очередях пула.
pub(crate) enum Job {
    /// `'static`-задача (`spawn`); сама перехватывает свою панику и сообщает результат.
    Boxed(Box<dyn FnOnce() + Send + 'static>),
    /// Задача области (`scope`/`join`/`parallel_*`) со стёртым в `pragma_sync` лайфтаймом.
    Scoped(ScopedTask),
    /// Вторая ветвь `join` на стеке владельца (DECISIONS #30).
    Stack(StackJob),
}

impl Job {
    /// Исполняет задачу; возвращает индекс участника пула, которого нужно разбудить
    /// (владелец `join`, уснувший в ожидании этой задачи).
    fn run(self) -> Option<usize> {
        match self {
            Self::Boxed(f) => f(),
            Self::Scoped(task) => task.run(),
            Self::Stack(job) => return job.run(),
        }
        None
    }
}

/// Сколько спит владелец `join`, ожидающий украденную ветвь, прежде чем снова поискать работу
/// (вор будит раньше, когда закончит).
const JOIN_RECHECK: Duration = Duration::from_micros(500);

/// Локальная очередь воркера: только `Frame` и `Normal` (фоновые задачи — только глобально).
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

/// Локальные очереди участника пула и его парковщик: переезжают вместе между внешним слотом
/// и потоком, который его занял.
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

/// Запуск воркеров: сколько ещё не закончили инициализацию потока (см. [`ThreadPool::new`]).
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

    /// Ждёт, пока каждый воркер не отметится в [`worker_ready`](Self::worker_ready).
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

/// Состояние пула, общее для владельца, воркеров и хэндлов.
pub(crate) struct Shared {
    frame: Injector<Job>,
    normal: Injector<Job>,
    background: Injector<Job>,
    /// Сначала воркеры (`0..workers`), затем внешние слоты (`workers..`).
    stealers: Vec<LocalStealers>,
    /// По одному на воркер; индекс совпадает с индексом в `stealers`.
    unparkers: Vec<Unparker>,
    /// Будильники всех участников (воркеры, затем внешние слоты) — для владельца `join`,
    /// уснувшего в ожидании украденной ветви.
    participants: Vec<Unparker>,
    /// Локальные очереди для внешних потоков (см. [`enter`]). `None` — слот занят.
    external_slots: Vec<Mutex<Option<LocalQueues>>>,
    /// Индексы спящих воркеров (стек: будим последнего уснувшего — его кэш теплее).
    idle: Mutex<Vec<usize>>,
    // Горячие счётчики — каждый на своей кэш-линии, чтобы не мешать друг другу.
    sleeping: CachePadded<AtomicUsize>,
    /// Воркеры, которые не спят и ищут работу (фаза spin): при их наличии новую задачу
    /// подберут без пробуждения спящих.
    searching: CachePadded<AtomicUsize>,
    background_active: CachePadded<AtomicUsize>,
    shutdown: AtomicBool,
    /// Потоки пула завершены и `join`-ены: задачи в очередях больше никто не исполнит.
    stopped: AtomicBool,
    policy: ShutdownPolicy,
    background_limit: usize,
    /// Раундов `yield` в фазе поиска перед парковкой (`PoolConfig::with_idle_spin`).
    idle_spin: u32,
    /// Метрики (см. [`PoolStats`]): по набору на каждый индекс `stealers` и последний — для
    /// внешних потоков без слота.
    counters: CounterSet,
    startup: Startup,
    #[cfg(debug_assertions)]
    watchdog: Option<Watchdog>,
}

thread_local! {
    /// Контекст воркера, если текущий поток — воркер какого-либо пула.
    static WORKER: Cell<Option<Rc<WorkerCtx>>> = const { Cell::new(None) };
    /// Приоритет исполняемой сейчас задачи (`None` — поток вне задачи пула).
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

/// Число внешних слотов: столько потоков вне пула одновременно ждут «как воркеры».
const EXTERNAL_SLOTS: usize = 4;

/// Выполняет `f`, сделав текущий поток временным участником пула.
///
/// Внешний поток (например, главный) занимает свободный внешний слот — собственную
/// LIFO-очередь, видимую ворам. Подзадачи `join`/`scope` попадают в неё, а ожидание берёт
/// сначала свежие свои задачи, как воркер. Без слота подзадачи ушли бы в FIFO-injector, и
/// помощь в ожидании брала бы самые старые (крупные) задачи, наращивая стек на каждом уровне.
/// Если все слоты заняты, поток работает через глобальные очереди (медленнее, но корректно).
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
    /// Контекст внешнего слота от прошлого [`enter`] этого потока, с очередями-заглушками
    /// вместо настоящих. Без него каждый вход главного потока в пул (раз в кадр) стоил бы
    /// аллокации `Rc` (DECISIONS #18). Держит `Arc<Shared>` последнего пула до следующего
    /// `enter` или выхода из потока: потоков пула это не задерживает, только память.
    static SPARE_EXTERNAL: Cell<Option<Rc<WorkerCtx>>> = const { Cell::new(None) };
}

/// Контекст внешнего потока с очередями слота `local` и заглушка, которую он вернёт на их
/// место при выходе (`None` — контекст новый, заглушку создаст [`ExternalGuard`]).
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

/// Возвращает внешний слот и восстанавливает прежний контекст потока (и при панике).
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
        // Оставшиеся в очереди задачи (например, `spawn` из тела `scope`) не теряются:
        // очередь возвращается в слот, её по-прежнему видят воры. На её место встаёт
        // заглушка, и контекст переиспользуется следующим `enter`.
        let placeholder = self.placeholder.take().unwrap_or_else(LocalQueues::new);
        let local = std::mem::replace(&mut inner.local, placeholder);
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(local);
        let _ = SPARE_EXTERNAL.try_with(|spare| spare.set(Some(ctx)));
    }
}

/// Приоритет для подзадач `scope`/`join`/`parallel_*`: наследуется от текущей задачи.
/// Вне пула — `Frame` (работу кадра запускает главный поток). Подзадачи фоновой задачи
/// идут в `Normal`: иначе они упёрлись бы в лимит `Background`, который держит их же родитель.
pub(crate) fn inherited_priority() -> Priority {
    match CURRENT_PRIORITY.try_with(Cell::get).ok().flatten() {
        None => Priority::Frame,
        Some(Priority::Background) => Priority::Normal,
        Some(other) => other,
    }
}

/// Приоритет задачи пула, которую исполняет текущий поток (`None` — поток вне задач пула).
pub(crate) fn current_priority() -> Option<Priority> {
    CURRENT_PRIORITY.try_with(Cell::get).ok().flatten()
}

/// Debug-сборки: отмечает задачу с меткой в ячейке watchdog текущего потока пула.
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

/// Исполняет задачу с выставленным текущим приоритетом. Паника, вырвавшаяся мимо обёртки
/// задачи (например, из деструктора захваченного значения), не убивает поток.
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
    // Ошибка доступа возможна только при разрушении TLS на выходе потока — тогда восстанавливать нечего.
    let _ = CURRENT_PRIORITY.try_with(|p| p.set(previous));
}

/// Сбрасывает невыполненную задачу. Деструктор захваченных значений (например, future
/// отменённой async-задачи) — пользовательский код: его паника не должна уронить поток,
/// который останавливает пул или планирует задачу.
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

    /// Кладёт отсоединённую задачу, если пул ещё принимает работу; иначе сбрасывает её
    /// (см. [`Spawner::spawn_detached`](crate::Spawner::spawn_detached)).
    pub(crate) fn push_detached(self: &Arc<Self>, job: Job, priority: Priority) -> bool {
        if self.is_shutdown() {
            drop_job(job);
            return false;
        }
        self.push(job, priority);
        // Гонка с `Drop` пула, который уже `join`-ил потоки: пара к `fence` перед
        // `drain_queues` в `Drop` — либо он увидит нашу задачу, либо мы увидим `stopped`.
        fence(Ordering::SeqCst);
        if self.stopped.load(Ordering::SeqCst) {
            self.drain_queues();
            return false;
        }
        true
    }

    /// Кладёт задачу в очередь и будит одного спящего воркера.
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
        // Пара к `fence` в `WorkerCtx::sleep`: либо воркер увидит задачу при перепроверке,
        // либо мы увидим его в `sleeping`.
        fence(Ordering::SeqCst);
        // Ищущий воркер заберёт задачу сам (а если уснёт, то перепроверит очереди перед
        // парковкой), поэтому системный вызов на пробуждение не нужен.
        if self.sleeping.load(Ordering::SeqCst) == 0 || self.searching.load(Ordering::SeqCst) > 0 {
            return;
        }
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

    /// Будит участника пула `index` (владельца `join`, уснувшего на своём парковщике).
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

    /// Поиск работы для потока, который не является воркером этого пула: только `Frame` и
    /// `Normal`. Фоновые задачи внешний поток не берёт — иначе главный поток мог бы надолго
    /// застрять в запекании посреди кадра.
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

    /// Метрики внешних потоков без слота (пишут несколько потоков — только RMW).
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

    /// Сбрасывает задачи глобальных очередей и свободных внешних слотов
    /// (их хэндлы получают `Cancelled`).
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

    /// Поток вызывающего — воркер этого пула или внешний поток внутри `scope`/`join`.
    pub(crate) fn in_context(self: &Arc<Self>) -> bool {
        current_worker_of(self).is_some()
    }
}

/// `join` через задачу на стеке, если текущий поток — участник пула `shared` (воркер или
/// внешний поток во временном слоте); `None` — не участник, `a` и `b` возвращаются нетронутыми.
#[expect(clippy::type_complexity, reason = "внутренний результат двух ветвей")]
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

/// Выполняет одну единицу чужой работы, если она есть (help-while-wait).
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
    /// Задача заняла слот `Background`, который нужно вернуть после исполнения.
    background_slot: bool,
}

struct WorkerCtx {
    shared: Arc<Shared>,
    index: usize,
    local: LocalQueues,
    rng: Cell<u64>,
    /// Воркер уже исполняет фоновую задачу: вложенные фоновые задачи (help-while-wait)
    /// идут в счёт того же слота, иначе при лимите 1 ожидание фоновой задачи внутри
    /// фоновой задачи зависло бы навсегда.
    holds_background: Cell<bool>,
    /// Внешний поток во временном слоте (см. [`enter`]): не берёт фоновые задачи и не спит.
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

    /// Перенастраивает кэшированный внешний контекст на слот `index` пула `shared`, как
    /// новый; возвращает очереди-заглушку, стоявшие на месте `local`.
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

    /// Метрики этого потока: пишет только он (воркер или владелец внешнего слота).
    fn counters(&self) -> Option<&Counters> {
        self.shared.counters.get(self.index).map(|c| &**c)
    }

    /// xorshift64: детерминированный выбор жертвы без системного RNG.
    fn next_random(&self) -> u64 {
        let mut x = self.rng.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.set(x);
        x
    }

    /// Порядок поиска: для `Frame`, затем для `Normal` — своя очередь → глобальная →
    /// кража у случайной жертвы; затем `Background` в пределах лимита.
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
                // `steal` в crossbeam сначала входит в эпоху (`epoch::pin`), а пустых жертв на
                // круге поиска почти все: дешёвая проверка длины без входа в эпоху.
                if i == self.index || stealers.get(lane).is_empty() {
                    continue;
                }
                // У жертвы крадём по одной (самой старой, обычно самой крупной) задаче:
                // кража пачкой уносит и мелкие `join`-ветви, которых владелец вот-вот
                // дождётся сам, — и вместо локального pop начинается пинг-понг между потоками.
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
        // Внешний поток (главный) не берёт фон: застрять в запекании посреди кадра хуже,
        // чем подождать.
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
        // До исполнения: к моменту, когда ожидающий увидит завершение задачи, счётчик уже
        // увеличен (снимок после `join` сходится с числом задач).
        if let Some(counters) = self.counters() {
            counters.executed();
        }
        execute(&self.shared, found.job, found.priority);
        if found.background_slot {
            self.holds_background.set(false);
            self.shared.release_background();
        }
    }

    /// Фаза spin: воркер не спит и ищет работу, отмечаясь в `searching`.
    fn search(&self) -> Option<Found> {
        self.shared.searching.fetch_add(1, Ordering::SeqCst);
        let backoff = Backoff::new();
        let mut yields = 0_u32;
        let mut found = None;
        loop {
            // Сначала короткий spin (`Backoff`), затем `idle_spin` раундов с `yield`: потоки,
            // которые вот-вот получат работу (следующая волна `join`, следующий кадр), не
            // платят за парковку и пробуждение через ОС (см. `docs/perf.md`).
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
        // Пока мы искали, пушеры не будили спящих; если работы больше, чем одна задача,
        // передаём эстафету следующему.
        // Сначала дешёвая проверка `sleeping`: сканировать чужие очереди, когда будить некого,
        // — лишние промахи кэша на горячем пути.
        if found.is_some()
            && self.shared.sleeping.load(Ordering::SeqCst) > 0
            && self.shared.has_visible_work()
        {
            self.shared.notify_one();
        }
        found
    }

    /// `join` на участнике пула (воркер или внешний слот): `b` — задача на стеке
    /// (DECISIONS #30). Не украденную `b` поток забирает из своей очереди и исполняет сам —
    /// без атомарных RMW и без аллокаций; украденную ждёт, помогая, а затем засыпает до
    /// пробуждения вором.
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
                self.shared.notify_one();
                let ra = panic::catch_unwind(AssertUnwindSafe(a));
                // `a` забрала из очереди всё, что положила сама (кроме `spawn` без ожидания),
                // поэтому сверху — наша `b`, если её не украли.
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

    /// Один шаг ожидания украденной ветви `join`: помочь, покрутиться или уснуть.
    fn wait_step(&self, latch: &JoinLatch<'_>, backoff: &Backoff) {
        if self.run_one() {
            backoff.reset();
        } else if !backoff.is_completed() {
            backoff.snooze();
        } else if latch.prepare_park() {
            // Вор разбудит по индексу (`Shared::wake_participant`); таймаут — чтобы снова
            // поискать работу, появившуюся, пока спим (новые задачи будят только воркеров).
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
        // Пара к `fence` в `Shared::notify_one`.
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

/// Платформенные настройки воркера (`sys`), применяемые им самим при старте.
#[derive(Clone, Copy)]
struct ThreadSetup {
    priority: Option<sys::ThreadPriority>,
    core: Option<usize>,
}

impl ThreadSetup {
    /// Отказ ОС — не повод не запускать воркер: предупреждение и работа без настройки.
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
    // Первая кража регистрирует поток в `crossbeam-epoch` (аллокация хэндла в TLS). Делаем её
    // сразу, а не посреди кадра: с проверкой `is_empty` перед кражей воркер может долго не
    // доходить до `steal` (кадр графа без аллокаций, `tests/alloc.rs`).
    if let Some(own) = shared.stealers.get(index) {
        let _ = own.frame.steal();
    }
    let ctx = Rc::new(WorkerCtx::new(shared, index, local, false));
    let _ = WORKER.try_with(|cell| cell.set(Some(Rc::clone(&ctx))));
    // Всё, что поток аллоцирует один раз (хэндл эпохи, контекст, список TLS-деструкторов),
    // уже выделено: `ThreadPool::new` может вернуть управление (DECISIONS #39).
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
    // Политика `Cancel`: задачи, оставшиеся в своей очереди, сбрасываются (хэндлы — `Cancelled`).
    ctx.local.drain();
}

/// Пул рабочих потоков с work-stealing.
///
/// Создание — [`ThreadPool::new`]; `Drop` останавливает и `join`-ит все потоки
/// (см. [`ShutdownPolicy`]). Паника в задаче не убивает воркер и возвращается как
/// [`JobError::Panicked`](crate::JobError::Panicked).
pub struct ThreadPool {
    pub(crate) shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    /// Поток watchdog (только debug-сборки и только если он включён в конфиге).
    #[cfg(debug_assertions)]
    watchdog: Option<JoinHandle<()>>,
}

/// Поток watchdog: просматривает ячейки потоков пула, пока пул не начал остановку.
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
    /// Запускает пул. Потоки называются `<name_prefix>-<index>`.
    ///
    /// Возвращается, когда каждый воркер закончил инициализацию потока (приоритет и affinity
    /// применены, одноразовые выделения потока сделаны): первые кадры не платят за запуск
    /// потоков, даже если воркерам не хватает ядер.
    ///
    /// Если ОС не дала создать поток, уже запущенные потоки останавливаются и `join`-ятся,
    /// а возвращается [`JobsError::SpawnFailed`].
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
                // `pool` сбрасывается здесь и останавливает уже запущенные потоки.
                Err(source) => return Err(JobsError::SpawnFailed { name, source }),
            }
        }
        // Иначе одноразовая инициализация воркера, которому под переподпиской не досталось
        // ядра, случилась бы посреди первых кадров (DECISIONS #39).
        pool.shared.startup.wait();
        #[cfg(debug_assertions)]
        pool.start_watchdog(config.name_prefix());
        Ok(pool)
    }

    /// Debug-сборки: запускает поток watchdog. Не вышло — пул работает без него.
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

    /// Снимок метрик пула: задачи, кражи, простой и парковки по воркерам, глубина очередей.
    ///
    /// Не блокирует воркеры (только чтение атомиков и длин очередей) и не влияет на
    /// исполнение задач; аллоцирует сам снимок.
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

    /// Индекс воркера этого пула, на котором выполняется вызов (`None` — поток не воркер
    /// этого пула, например главный поток внутри `scope`).
    #[must_use]
    pub fn current_worker_index(&self) -> Option<usize> {
        current_worker_of(&self.shared)
            .filter(|w| !w.external)
            .map(|w| w.index)
    }

    /// Число рабочих потоков.
    #[must_use]
    pub fn workers(&self) -> usize {
        self.threads.len()
    }

    /// Текущий поток — воркер этого пула.
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
                // Пул сброшен изнутри собственной задачи: себя не join-им, поток завершится
                // сам после текущей задачи.
                joined_all = false;
                continue;
            }
            if handle.join().is_err() {
                tracing::error!("pragma worker thread terminated with a panic");
            }
        }
        if joined_all {
            self.shared.stopped.store(true, Ordering::SeqCst);
            // Пара к `fence` в `Shared::push_detached`.
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

    /// Воркеры, которым не хватило ядер (больше воркеров, чем ядер), к возврату `new` уже
    /// инициализированы: их одноразовые выделения не попадают в первые кадры.
    #[test]
    fn new_returns_after_every_worker_is_initialized() {
        let workers = thread::available_parallelism().map_or(4, |n| n.get().saturating_mul(2));
        for _ in 0..20 {
            let pool = ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap();
            assert_eq!(pool.shared.startup.pending(), 0);
        }
    }
}
