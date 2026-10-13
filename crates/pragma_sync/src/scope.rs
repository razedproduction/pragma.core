use std::cell::UnsafeCell;
use std::fmt;
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};

use crate::AtomicCounter;
use crate::primitives::Arc;
use crate::primitives::{AtomicBool, Ordering};

type ErasedFn = Box<dyn FnOnce() + Send + 'static>;

/// Область, внутри которой задачи могут заимствовать данные со стека (без `'static`).
///
/// Создаётся только функцией [`scope`]; та не вернётся (ни нормально, ни паникой), пока
/// не завершится или не будет сброшена каждая задача, созданная через [`task`](Self::task)
/// или [`task_in`](Self::task_in). Именно это делает безопасным стирание лайфтайма в
/// [`ScopedTask`].
///
/// `D` — данные планировщика, доступные задачам по ссылке на всё время области
/// (например, ссылка на слот для первой ошибки). `TaskScope` ковариантен по `D`, поэтому
/// `D` может заимствовать локальные данные вызывающего: `&'scope TaskScope<'scope, 'env, X<'a>>`
/// сужается до `&'scope TaskScope<'scope, 'env, X<'scope>>`.
pub struct TaskScope<'scope, 'env: 'scope, D> {
    pending: Arc<AtomicCounter>,
    data: D,
    // Инвариантность по обоим лайфтаймам — как у `std::thread::Scope`. `'env` нужен ради
    // неявной границы `'env: 'scope` в `for<'scope>`-замыкании `body`: без неё компилятор
    // требовал бы `'static` от всего, что захватывают задачи.
    scope: PhantomData<&'scope mut &'scope ()>,
    env: PhantomData<&'env mut &'env ()>,
}

/// Слот для задачи на стеке вызывающего: задача из слота не аллоцирует (см.
/// [`TaskScope::task_in`]). Для горячих путей вроде `join`, где `Box` на каждую задачу
/// стоит дороже самой работы.
pub struct StackSlot<F> {
    /// Слот уже отдан задаче: доступ к `f` получает ровно одна [`ScopedTask`].
    claimed: AtomicBool,
    f: UnsafeCell<Option<F>>,
}

// SAFETY: `f` читается и пишется только через `ScopedTask`, которой слот отдан (`claimed`
// гарантирует, что такая задача одна); владелец слота доступа к `f` не имеет. Замыкание
// уходит в другой поток, поэтому требуется `F: Send`.
unsafe impl<F: Send> Sync for StackSlot<F> {}

impl<F> StackSlot<F> {
    /// Слот с замыканием `f`.
    #[must_use]
    pub fn new(f: F) -> Self {
        Self {
            claimed: AtomicBool::new(false),
            f: UnsafeCell::new(Some(f)),
        }
    }
}

impl<F> fmt::Debug for StackSlot<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StackSlot")
            .field("claimed", &self.claimed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Задача области со стёртым лайфтаймом: её можно положить в любую `'static`-очередь.
///
/// Выполняется через [`run`](Self::run); сброс без выполнения тоже засчитывается как
/// завершение, поэтому область не зависнет из-за отменённых задач. Утечка через
/// `mem::forget` безопасна, но подвесит ожидание области навсегда.
pub struct ScopedTask {
    body: TaskBody,
    // Объявлено после `body`: при сбросе задачи захваченные значения уничтожаются раньше,
    // чем область узнаёт о завершении, — даже если деструктор захваченного значения паникует.
    // Поле не читается: вся работа — в `Drop`.
    _done: DoneGuard,
}

enum TaskBody {
    Boxed(Option<ErasedFn>),
    /// Замыкание лежит в [`StackSlot`] на стеке владельца области.
    Slot {
        slot: *const (),
        /// Мономорфизированный `call_slot::<F>`.
        call: unsafe fn(*const (), bool),
        pending: bool,
    },
}

impl Drop for TaskBody {
    fn drop(&mut self) {
        if let Self::Slot {
            slot,
            call,
            pending,
        } = self
            && *pending
        {
            *pending = false;
            // SAFETY: `slot` создан в `task_in` из `&'scope StackSlot<F>` с `call_slot::<F>`;
            // слот жив, пока задача не завершена (область ждёт её), и задача — единственный
            // владелец доступа к нему. `pending` гарантирует однократный вызов.
            unsafe { call(*slot, false) };
        }
    }
}

/// Забирает замыкание из слота и выполняет (`run`) или уничтожает его.
///
/// # Safety
/// `ptr` — указатель на живой `StackSlot<F>`, доступ к которому принадлежит вызывающей задаче.
unsafe fn call_slot<F: FnOnce()>(ptr: *const (), run: bool) {
    // SAFETY: по контракту функции `ptr` указывает на живой `StackSlot<F>`.
    let slot = unsafe { &*ptr.cast::<StackSlot<F>>() };
    // SAFETY: к `f` обращается только задача, которой отдан слот (`claimed`), и только здесь;
    // других ссылок на содержимое ячейки нет.
    let f = unsafe { (*slot.f.get()).take() };
    if run {
        if let Some(f) = f {
            f();
        }
    } else {
        drop(f);
    }
}

// SAFETY: `Boxed` содержит `Send`-замыкание. `Slot` указывает на `StackSlot<F>` с `F: Send`
// (требование `task_in`), который `Sync` и живёт, пока задача не завершена; перенос задачи
// в другой поток передаёт туда только эксклюзивный доступ к этому слоту.
unsafe impl Send for ScopedTask {}

struct DoneGuard(Arc<AtomicCounter>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.0.decrement();
    }
}

impl ScopedTask {
    /// Выполняет задачу. Паника замыкания пробрасывается вызывающему, но область всё равно
    /// получает сигнал о завершении.
    pub fn run(mut self) {
        match &mut self.body {
            TaskBody::Boxed(f) => {
                if let Some(f) = f.take() {
                    f();
                }
            }
            TaskBody::Slot {
                slot,
                call,
                pending,
            } => {
                if *pending {
                    // Сбрасываем до вызова: при панике `Drop` не тронет слот повторно.
                    *pending = false;
                    // SAFETY: см. `TaskBody::drop` — те же инварианты, вызов однократный.
                    unsafe { call(*slot, true) };
                }
            }
        }
    }
}

impl fmt::Debug for ScopedTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pending = match &self.body {
            TaskBody::Boxed(f) => f.is_some(),
            TaskBody::Slot { pending, .. } => *pending,
        };
        f.debug_struct("ScopedTask")
            .field("pending", &pending)
            .finish_non_exhaustive()
    }
}

impl<D: fmt::Debug> fmt::Debug for TaskScope<'_, '_, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskScope")
            .field("pending", &self.pending.get())
            .field("data", &self.data)
            .finish()
    }
}

impl<'scope, D> TaskScope<'scope, '_, D> {
    /// Данные планировщика, переданные в [`scope`].
    #[must_use]
    pub fn data(&self) -> &D {
        &self.data
    }

    /// Сколько задач области ещё не завершено.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.get()
    }

    fn register(&self) -> DoneGuard {
        if self.pending.increment().is_err() {
            // Недостижимо: каждая живая задача держит клон `Arc` счётчика, а `Arc`
            // аварийно завершает процесс при переполнении своего счётчика ссылок
            // (isize::MAX) раньше, чем этот счётчик дойдёт до usize::MAX. Без инкремента
            // стирать лайфтайм нельзя, поэтому и здесь — abort, а не тихое продолжение.
            std::process::abort();
        }
        DoneGuard(Arc::clone(&self.pending))
    }

    /// Упаковывает замыкание в задачу области; область будет ждать её завершения или сброса.
    pub fn task<F>(&self, f: F) -> ScopedTask
    where
        F: FnOnce() + Send + 'scope,
    {
        let done = self.register();
        let boxed: Box<dyn FnOnce() + Send + 'scope> = Box::new(f);
        // SAFETY: меняется только лайфтайм trait-объекта, раскладка `Box<dyn ...>` та же.
        // Замыкание живёт не дольше `'scope`: счётчик `pending` увеличен выше и уменьшается
        // только в `DoneGuard::drop` — после того как замыкание выполнено или уничтожено.
        // `scope()` не возвращает управление (ни нормально, ни паникой), пока `pending`
        // не равен нулю, а если ожидание прервано паникой — аварийно завершает процесс.
        // `'scope` — заимствование локальной `TaskScope` внутри `scope()`, поэтому оно
        // заканчивается до возврата, а всё, что захвачено с лайфтаймом `>= 'scope`, ещё живо.
        let erased: ErasedFn =
            unsafe { std::mem::transmute::<Box<dyn FnOnce() + Send + 'scope>, ErasedFn>(boxed) };
        ScopedTask {
            body: TaskBody::Boxed(Some(erased)),
            _done: done,
        }
    }

    /// Задача из слота на стеке, без аллокации. Слот должен пережить область (`&'scope`);
    /// каждый слот отдаётся только одной задаче — повторный вызов вернёт `None`.
    pub fn task_in<F>(&self, slot: &'scope StackSlot<F>) -> Option<ScopedTask>
    where
        F: FnOnce() + Send + 'scope,
    {
        if slot.claimed.swap(true, Ordering::AcqRel) {
            return None;
        }
        let done = self.register();
        // Указатель живёт столько же, сколько заимствование `&'scope StackSlot<F>`: слот
        // переживает область, а область ждёт завершения задачи (см. SAFETY в `task`).
        Some(ScopedTask {
            body: TaskBody::Slot {
                slot: std::ptr::from_ref(slot).cast::<()>(),
                call: call_slot::<F>,
                pending: true,
            },
            _done: done,
        })
    }
}

/// Счётчики завершившихся областей, готовые к повторному использованию: аллокация на
/// каждый `join` стоила бы дороже самого `join`.
#[cfg(not(loom))]
mod counter_cache {
    use std::cell::RefCell;

    use crate::AtomicCounter;
    use crate::primitives::Arc;

    const CAPACITY: usize = 64;

    thread_local! {
        static SPARE: RefCell<Vec<Arc<AtomicCounter>>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn take() -> Arc<AtomicCounter> {
        SPARE
            .try_with(|spare| spare.try_borrow_mut().ok().and_then(|mut s| s.pop()))
            .ok()
            .flatten()
            .unwrap_or_else(|| Arc::new(AtomicCounter::new(0)))
    }

    /// Вызывается, пока область ещё владеет счётчиком.
    pub(super) fn give_back(counter: &Arc<AtomicCounter>) {
        // Переиспользуем, только если клонов, кроме ссылки самой области, нет: значит, ни одна
        // задача (и ни один `decrement`, ещё держащий `Arc`) его не касается, а значение — ноль.
        if Arc::strong_count(counter) != 1 || !counter.is_zero() {
            return;
        }
        let counter = Arc::clone(counter);
        let _ = SPARE.try_with(|spare| {
            if let Ok(mut spare) = spare.try_borrow_mut()
                && spare.len() < CAPACITY
            {
                spare.push(counter);
            }
        });
    }
}

#[cfg(loom)]
mod counter_cache {
    use crate::AtomicCounter;
    use crate::primitives::Arc;

    pub(super) fn take() -> Arc<AtomicCounter> {
        Arc::new(AtomicCounter::new(0))
    }

    pub(super) fn give_back(_counter: &Arc<AtomicCounter>) {}
}

/// Запускает область задач с заимствованиями.
///
/// `body` создаёт задачи через [`TaskScope::task`] и отдаёт их исполнителю. После `body`
/// функция ждёт завершения всех задач, вызывая `help` (help-while-wait, см.
/// [`AtomicCounter::wait_while_helping`]) и возвращает результат `body`. Чтобы прочитать
/// результаты задач после области (например, собранные ошибки), положите в `data` ссылку
/// на свои локальные данные.
///
/// Паника в `body` пробрасывается после того, как все задачи завершены. Паника в `help`
/// приводит к `abort`: продолжать раскрутку стека, пока задачи держат ссылки на него, нельзя.
pub fn scope<'env, D, T>(
    data: D,
    body: impl for<'scope> FnOnce(&'scope TaskScope<'scope, 'env, D>) -> T,
    mut help: impl FnMut() -> bool,
) -> T {
    let scope = TaskScope {
        pending: counter_cache::take(),
        data,
        scope: PhantomData,
        env: PhantomData,
    };
    let result = panic::catch_unwind(AssertUnwindSafe(|| body(&scope)));
    let waited = panic::catch_unwind(AssertUnwindSafe(|| {
        scope.pending.wait_while_helping(&mut help)
    }));
    if waited.is_err() {
        std::process::abort();
    }
    counter_cache::give_back(&scope.pending);
    match result {
        Ok(value) => value,
        Err(payload) => panic::resume_unwind(payload),
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Простейший исполнитель: задачи уходят в канал, их выполняют другие потоки.
    #[test]
    fn tasks_borrow_stack_data_across_threads() {
        let mut data = vec![0_u64; 64];
        let (tx, rx) = std::sync::mpsc::channel::<ScopedTask>();
        let rx = Mutex::new(rx);
        std::thread::scope(|ts| {
            for _ in 0..4 {
                ts.spawn(|| {
                    while let Ok(task) = rx.lock().unwrap().recv() {
                        task.run();
                    }
                });
            }
            scope(
                (),
                |s| {
                    for (i, chunk) in data.chunks_mut(8).enumerate() {
                        let task = s.task(move || {
                            for x in chunk.iter_mut() {
                                *x = i as u64;
                            }
                        });
                        tx.send(task).unwrap();
                    }
                },
                || false,
            );
            drop(tx);
        });
        for (i, chunk) in data.chunks(8).enumerate() {
            assert!(chunk.iter().all(|&x| x == i as u64));
        }
    }

    #[test]
    fn dropped_task_counts_as_finished() {
        let ran = AtomicUsize::new(0);
        scope(
            (),
            |s| {
                let task = s.task(|| {
                    ran.fetch_add(1, Ordering::SeqCst);
                });
                assert_eq!(s.pending(), 1);
                drop(task);
                assert_eq!(s.pending(), 0);
            },
            || false,
        );
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn help_runs_pending_tasks() {
        let queue: Mutex<Vec<ScopedTask>> = Mutex::new(Vec::new());
        let sum = AtomicUsize::new(0);
        let extra = scope(
            7_usize,
            |s| {
                for i in 0..10 {
                    let sum = &sum;
                    let extra = *s.data();
                    queue.lock().unwrap().push(s.task(move || {
                        sum.fetch_add(i + extra, Ordering::SeqCst);
                    }));
                }
                *s.data()
            },
            || match queue.lock().unwrap().pop() {
                Some(task) => {
                    task.run();
                    true
                }
                None => false,
            },
        );
        assert_eq!(extra, 7);
        assert_eq!(sum.load(Ordering::SeqCst), 45 + 70);
    }

    #[test]
    fn body_panic_waits_for_tasks_then_propagates() {
        let queue: Mutex<Vec<ScopedTask>> = Mutex::new(Vec::new());
        let ran = AtomicUsize::new(0);
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            scope(
                (),
                |s| {
                    queue.lock().unwrap().push(s.task(|| {
                        ran.fetch_add(1, Ordering::SeqCst);
                    }));
                    panic!("body failed");
                },
                || match queue.lock().unwrap().pop() {
                    Some(task) => {
                        task.run();
                        true
                    }
                    None => false,
                },
            )
        }));
        assert!(result.is_err());
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "task finished before unwinding"
        );
    }

    #[test]
    fn stack_slot_task_runs_on_other_thread() {
        let mut out = 0_u64;
        let slot = StackSlot::new(|| out = 42);
        std::thread::scope(|ts| {
            scope(
                (),
                |s| {
                    let task = s.task_in(&slot).unwrap();
                    assert!(s.task_in(&slot).is_none(), "slot is handed out only once");
                    ts.spawn(move || task.run());
                },
                || false,
            );
        });
        // Слот держит `&mut out` до конца своей жизни.
        let _ = slot;
        assert_eq!(out, 42);
    }

    #[test]
    fn stack_slot_dropped_task_drops_closure_once() {
        let tracker = Arc::new(());
        let captured = Arc::clone(&tracker);
        let slot = StackSlot::new(move || drop(captured));
        scope(
            (),
            |s| {
                let task = s.task_in(&slot).unwrap();
                assert_eq!(s.pending(), 1);
                drop(task);
                assert_eq!(s.pending(), 0);
            },
            || false,
        );
        assert_eq!(
            Arc::strong_count(&tracker),
            1,
            "closure dropped without running"
        );
        drop(slot);
    }

    #[test]
    fn stack_slot_panic_signals_completion() {
        let slot = StackSlot::new(|| panic!("slot task failed"));
        scope(
            (),
            |s| {
                let task = s.task_in(&slot).unwrap();
                assert!(panic::catch_unwind(AssertUnwindSafe(|| task.run())).is_err());
                assert_eq!(s.pending(), 0);
            },
            || false,
        );
    }

    #[test]
    fn counters_are_reused_between_scopes() {
        for _ in 0..3 {
            scope(
                (),
                |s| {
                    let task = s.task(|| {});
                    task.run();
                },
                || false,
            );
        }
        let reused = counter_cache::take();
        assert!(reused.is_zero());
        assert_eq!(Arc::strong_count(&reused), 1);
    }

    #[test]
    fn task_panic_still_signals_completion() {
        scope(
            (),
            |s| {
                let task = s.task(|| panic!("task failed"));
                let r = panic::catch_unwind(AssertUnwindSafe(|| task.run()));
                assert!(r.is_err());
                assert_eq!(s.pending(), 0);
            },
            || false,
        );
    }
}
