use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_deque::{Injector, Steal, Stealer, Worker as Deque};
use pragma_base::tracing;
use pragma_sync::{Backoff, CachePadded, Parker, ScopedTask, Unparker};

use crate::{JobsError, PoolConfig, Priority, ShutdownPolicy};

/// Страховка от ошибок протокола пробуждения: спящий воркер всё равно проверит очереди.
const PARK_TIMEOUT: Duration = Duration::from_millis(50);

/// Единица работы в очередях пула.
pub(crate) enum Job {
    /// `'static`-задача (`spawn`); сама перехватывает свою панику и сообщает результат.
    Boxed(Box<dyn FnOnce() + Send + 'static>),
    /// Задача области (`scope`/`join`/`parallel_*`) со стёртым в `pragma_sync` лайфтаймом.
    Scoped(ScopedTask),
}

impl Job {
    fn run(self) {
        match self {
            Self::Boxed(f) => f(),
            Self::Scoped(task) => task.run(),
        }
    }
}

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

struct LocalQueues {
    frame: Deque<Job>,
    normal: Deque<Job>,
}

impl LocalQueues {
    fn new() -> Self {
        Self {
            frame: Deque::new_lifo(),
            normal: Deque::new_lifo(),
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
                drop(job);
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

/// Состояние пула, общее для владельца, воркеров и хэндлов.
pub(crate) struct Shared {
    frame: Injector<Job>,
    normal: Injector<Job>,
    background: Injector<Job>,
    /// Сначала воркеры (`0..workers`), затем внешние слоты (`workers..`).
    stealers: Vec<LocalStealers>,
    /// По одному на воркер; индекс совпадает с индексом в `stealers`.
    unparkers: Vec<Unparker>,
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
    policy: ShutdownPolicy,
    background_limit: usize,
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

/// Исполняет задачу с выставленным текущим приоритетом. Паника, вырвавшаяся мимо обёртки
/// задачи (например, из деструктора захваченного значения), не убивает поток.
fn execute(job: Job, priority: Priority) {
    let previous = CURRENT_PRIORITY
        .try_with(|p| p.replace(Some(priority)))
        .ok()
        .flatten();
    if panic::catch_unwind(AssertUnwindSafe(|| job.run())).is_err() {
        tracing::error!("panic escaped a job wrapper; worker keeps running");
    }
    // Ошибка доступа возможна только при разрушении TLS на выходе потока — тогда восстанавливать нечего.
    let _ = CURRENT_PRIORITY.try_with(|p| p.set(previous));
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

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
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
                    return Some((job, priority));
                }
            }
        }
        None
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
                drop(job);
            }
        }
        for slot in &self.external_slots {
            if let Some(local) = slot.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                local.drain();
            }
        }
    }
}

/// Выполняет одну единицу чужой работы, если она есть (help-while-wait).
pub(crate) fn help_once(shared: &Arc<Shared>) -> bool {
    if let Some(worker) = current_worker_of(shared) {
        return worker.run_one();
    }
    match shared.find_external() {
        Some((job, priority)) => {
            execute(job, priority);
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
                if i == self.index {
                    continue;
                }
                // У жертвы крадём по одной (самой старой, обычно самой крупной) задаче:
                // кража пачкой уносит и мелкие `join`-ветви, которых владелец вот-вот
                // дождётся сам, — и вместо локального pop начинается пинг-понг между потоками.
                match stealers.get(lane).steal() {
                    Steal::Success(job) => return Some(job),
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
        execute(found.job, found.priority);
        if found.background_slot {
            self.holds_background.set(false);
            self.shared.release_background();
        }
    }

    /// Фаза spin: воркер не спит и ищет работу, отмечаясь в `searching`.
    fn search(&self) -> Option<Found> {
        self.shared.searching.fetch_add(1, Ordering::SeqCst);
        let backoff = Backoff::new();
        let mut found = None;
        while !backoff.is_completed() {
            backoff.snooze();
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

    fn sleep(&self, parker: &Parker) {
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
            parker.park_timeout(PARK_TIMEOUT);
        }
        self.shared.unregister_idle(self.index);
    }
}

fn worker_main(shared: Arc<Shared>, index: usize, local: LocalQueues, parker: Parker) {
    let ctx = Rc::new(WorkerCtx::new(shared, index, local, false));
    let _ = WORKER.try_with(|cell| cell.set(Some(Rc::clone(&ctx))));
    loop {
        if ctx.shared.policy == ShutdownPolicy::Cancel && ctx.shared.is_shutdown() {
            break;
        }
        if ctx.run_one() {
            continue;
        }
        if let Some(found) = ctx.search() {
            ctx.run_found(found);
            continue;
        }
        if ctx.shared.is_shutdown() {
            break;
        }
        ctx.sleep(&parker);
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
    /// Если ОС не дала создать поток, уже запущенные потоки останавливаются и `join`-ятся,
    /// а возвращается [`JobsError::SpawnFailed`].
    pub fn new(config: PoolConfig) -> Result<Self, JobsError> {
        let config = config.validate()?;
        let workers = config.workers();
        let mut locals = Vec::with_capacity(workers);
        let mut parkers = Vec::with_capacity(workers);
        let mut stealers = Vec::with_capacity(workers);
        let mut unparkers = Vec::with_capacity(workers);
        for _ in 0..workers {
            let local = LocalQueues::new();
            stealers.push(local.stealers());
            locals.push(local);
            let parker = Parker::new();
            unparkers.push(parker.unparker());
            parkers.push(parker);
        }
        let external_slots = (0..EXTERNAL_SLOTS)
            .map(|_| {
                let local = LocalQueues::new();
                stealers.push(local.stealers());
                Mutex::new(Some(local))
            })
            .collect();
        let shared = Arc::new(Shared {
            frame: Injector::new(),
            normal: Injector::new(),
            background: Injector::new(),
            stealers,
            unparkers,
            external_slots,
            idle: Mutex::new(Vec::with_capacity(workers)),
            sleeping: CachePadded::new(AtomicUsize::new(0)),
            searching: CachePadded::new(AtomicUsize::new(0)),
            background_active: CachePadded::new(AtomicUsize::new(0)),
            shutdown: AtomicBool::new(false),
            policy: config.shutdown(),
            background_limit: config.background_limit(),
        });
        let mut pool = Self {
            shared,
            threads: Vec::with_capacity(workers),
        };
        for (index, (local, parker)) in locals.into_iter().zip(parkers).enumerate() {
            let name = format!("{}-{index}", config.name_prefix());
            let mut builder = thread::Builder::new().name(name.clone());
            if let Some(bytes) = config.stack_size() {
                builder = builder.stack_size(bytes);
            }
            let shared = Arc::clone(&pool.shared);
            match builder.spawn(move || worker_main(shared, index, local, parker)) {
                Ok(handle) => pool.threads.push(handle),
                // `pool` сбрасывается здесь и останавливает уже запущенные потоки.
                Err(source) => return Err(JobsError::SpawnFailed { name, source }),
            }
        }
        Ok(pool)
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
        if joined_all || self.shared.policy == ShutdownPolicy::Cancel {
            self.shared.drain_queues();
        }
    }
}
