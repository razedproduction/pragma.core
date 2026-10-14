//! Fork-join без аллокаций и без `Arc`: вторая ветвь `join` — кадр на стеке владельца с
//! латчем (DECISIONS #30).
//!
//! Владелец кладёт [`StackJob`] в свою очередь, исполняет первую ветвь и забирает задачу
//! обратно. Не украденную задачу он исполняет сам ([`StackJob::run_inline`]) — ни одной
//! атомарной RMW. Украденную исполняет вор ([`StackJob::run`]): записывает результат и
//! ставит латч одним `swap`; если владелец к этому моменту собрался спать
//! ([`JoinLatch::prepare_park`]), `run` возвращает индекс владельца — разбудить его должен
//! вызывающий. После `swap` вор память кадра не трогает: владелец может сразу вернуться.

use std::cell::UnsafeCell;
use std::panic::{self, AssertUnwindSafe};
use std::{fmt, ptr, thread};

use crate::primitives::{AtomicUsize, Ordering};

const PENDING: usize = 0;
/// Владелец собирается спать: исполнитель должен его разбудить.
const SLEEPY: usize = 1;
const SET: usize = 2;

/// Индекс владельца, которого некому будить (у него нет парковщика): `run` никогда не
/// вернёт его, владелец перепроверяет латч сам.
pub const NO_OWNER: usize = usize::MAX;

/// Кадр второй ветви на стеке [`join_on_stack`].
struct Frame<F, R> {
    latch: AtomicUsize,
    owner: usize,
    /// Читает и пишет только исполнитель задачи (он один — задача не клонируется).
    f: UnsafeCell<Option<F>>,
    /// Пишет исполнитель до установки латча, читает владелец после.
    result: UnsafeCell<Option<thread::Result<R>>>,
}

#[derive(Clone, Copy)]
enum Mode {
    /// Исполняет владелец — латч ставится обычной записью.
    Owner,
    /// Исполняет другой поток.
    Thief,
    /// Задача сброшена без выполнения.
    Cancel,
}

/// Вторая ветвь `join` со стёртым типом и лайфтаймом: её можно положить в `'static`-очередь.
///
/// Исполняется через [`run`](Self::run) или [`run_inline`](Self::run_inline); сброс без
/// выполнения тоже ставит латч, поэтому владелец не зависнет. Утечка через `mem::forget`
/// безопасна, но подвесит владельца навсегда.
pub struct StackJob {
    frame: *const (),
    /// Мономорфизированный `call::<F, R>`.
    call: unsafe fn(*const (), Mode) -> Option<usize>,
    pending: bool,
}

// SAFETY: кадр требует `F: Send` и `R: Send` (см. `join_on_stack`); перенос задачи в другой
// поток передаёт туда единственный доступ к замыканию и слоту результата, а латч — атомик.
unsafe impl Send for StackJob {}

impl fmt::Debug for StackJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StackJob")
            .field("id", &self.id())
            .field("pending", &self.pending)
            .finish()
    }
}

impl StackJob {
    /// Идентификатор задачи (адрес кадра): по нему владелец узнаёт свою задачу в очереди.
    #[must_use]
    pub fn id(&self) -> usize {
        self.frame.addr()
    }

    /// Исполняет задачу на чужом потоке. Паника замыкания перехватывается и передаётся
    /// владельцу как результат. Возвращает индекс владельца, если его нужно разбудить.
    #[must_use = "владельца, собравшегося спать, нужно разбудить"]
    pub fn run(mut self) -> Option<usize> {
        self.pending = false;
        // SAFETY: `frame` и `call` созданы в `join_on_stack` для одного кадра; кадр жив, пока
        // латч не поставлен (владелец ждёт его), а `pending` гарантирует однократный вызов.
        unsafe { (self.call)(self.frame, Mode::Thief) }
    }

    /// Исполняет задачу на потоке владельца (вызвавшего [`join_on_stack`]) — без атомарных
    /// RMW. С другого потока корректно, но не разбудит спящего владельца (тот проснётся по
    /// таймауту своей парковки).
    pub fn run_inline(mut self) {
        self.pending = false;
        // SAFETY: см. `run`.
        let _ = unsafe { (self.call)(self.frame, Mode::Owner) };
    }
}

impl Drop for StackJob {
    fn drop(&mut self) {
        if self.pending {
            self.pending = false;
            // SAFETY: см. `run`. Будить владельца отсюда нечем: он проснётся по таймауту.
            let _ = unsafe { (self.call)(self.frame, Mode::Cancel) };
        }
    }
}

/// Исполняет (или сбрасывает) замыкание кадра и ставит латч.
///
/// # Safety
/// `ptr` — указатель на живой `Frame<F, R>`, вызов для этого кадра — первый и единственный.
unsafe fn call<F: FnOnce() -> R, R>(ptr: *const (), mode: Mode) -> Option<usize> {
    // SAFETY: по контракту функции.
    let frame = unsafe { &*ptr.cast::<Frame<F, R>>() };
    // Читаем до установки латча: после неё кадр может быть уже уничтожен.
    let owner = frame.owner;
    // SAFETY: к `f` обращается только этот (единственный) вызов.
    let f = unsafe { (*frame.f.get()).take() };
    match (mode, f) {
        (Mode::Cancel, f) => {
            // Деструктор захваченного — пользовательский код: его паника не должна
            // вырваться мимо латча.
            let _ = panic::catch_unwind(AssertUnwindSafe(|| drop(f)));
        }
        (_, Some(f)) => {
            let result = panic::catch_unwind(AssertUnwindSafe(f));
            // SAFETY: владелец читает `result` только после того, как увидит латч (ниже);
            // других писателей нет.
            unsafe { *frame.result.get() = Some(result) };
        }
        (_, None) => {}
    }
    match mode {
        Mode::Owner => {
            frame.latch.store(SET, Ordering::Release);
            None
        }
        Mode::Thief | Mode::Cancel => {
            let previous = frame.latch.swap(SET, Ordering::AcqRel);
            (previous == SLEEPY && owner != NO_OWNER).then_some(owner)
        }
    }
}

/// Латч второй ветви — для шага ожидания владельца (см. [`join_on_stack`]).
#[derive(Debug)]
pub struct JoinLatch<'a> {
    state: &'a AtomicUsize,
}

impl JoinLatch<'_> {
    /// Вторая ветвь завершена (или сброшена).
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.state.load(Ordering::Acquire) == SET
    }

    /// Объявляет, что владелец собирается спать на своём парковщике. `true` — можно
    /// парковаться: исполнитель вернёт из [`StackJob::run`] индекс владельца, чтобы его
    /// разбудили; `false` — латч уже поставлен, спать не нужно.
    ///
    /// Пробуждение не теряется: и объявление, и установка латча — RMW одного атомика, поэтому
    /// либо исполнитель видит `SLEEPY` и будит, либо владелец видит `SET` (loom-модель).
    #[must_use]
    pub fn prepare_park(&self) -> bool {
        match self
            .state
            .compare_exchange(PENDING, SLEEPY, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(state) => state != SET,
        }
    }
}

/// Fork-join: создаёт на стеке задачу для `b` и передаёт её в `body`.
///
/// `body` кладёт задачу в очередь (её исполнят [`StackJob::run`] или, если владелец заберёт
/// её обратно, [`StackJob::run_inline`]), исполняет первую ветвь и возвращает её результат.
/// После `body` функция ждёт латч, вызывая `wait` (шаг help-while-wait: исполнить чужую
/// задачу, покрутиться или уснуть после [`JoinLatch::prepare_park`]), и возвращает результат
/// `body` и `b`: `None` — задача сброшена без выполнения, `Some(Err(_))` — `b` запаниковала.
///
/// Паника в `body` пробрасывается после того, как задача завершена. Паника в `wait` приводит
/// к `abort`: продолжать раскрутку, пока задача ссылается на стек, нельзя.
///
/// `owner` — индекс владельца для пробуждения (или [`NO_OWNER`]).
///
/// ```
/// use pragma_sync::__private::{NO_OWNER, join_on_stack};
///
/// let mut sum = 0_u64;
/// let ((), b) = join_on_stack(
///     NO_OWNER,
///     || (1..=10_u64).sum::<u64>(),
///     |job| {
///         sum = 100;          // первая ветвь
///         job.run_inline();   // никто не украл — исполняем сами
///     },
///     |_latch| unreachable!("латч уже поставлен"),
/// );
/// assert_eq!((sum, b.and_then(Result::ok)), (100, Some(55)));
/// ```
pub fn join_on_stack<B, RB, T>(
    owner: usize,
    b: B,
    body: impl FnOnce(StackJob) -> T,
    mut wait: impl FnMut(&JoinLatch<'_>),
) -> (T, Option<thread::Result<RB>>)
where
    B: FnOnce() -> RB + Send,
    RB: Send,
{
    let frame = Frame {
        latch: AtomicUsize::new(PENDING),
        owner,
        f: UnsafeCell::new(Some(b)),
        result: UnsafeCell::new(None),
    };
    // SAFETY-инвариант (используется в `StackJob`): кадр не перемещается и живёт до конца
    // функции, а функция не возвращается (ни нормально, ни паникой), пока латч не поставлен —
    // после этого исполнитель кадр не трогает.
    let job = StackJob {
        frame: ptr::from_ref(&frame).cast::<()>(),
        call: call::<B, RB>,
        pending: true,
    };
    let out = panic::catch_unwind(AssertUnwindSafe(|| body(job)));
    let latch = JoinLatch {
        state: &frame.latch,
    };
    let waited = panic::catch_unwind(AssertUnwindSafe(|| {
        while !latch.is_set() {
            wait(&latch);
        }
    }));
    if waited.is_err() {
        std::process::abort();
    }
    let result = frame.result.into_inner();
    match out {
        Ok(value) => (value, result),
        Err(payload) => {
            drop(result);
            panic::resume_unwind(payload)
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    #[test]
    fn inline_run_needs_no_wait() {
        let mut data = [0_u32; 4];
        let (left, right) = data.split_at_mut(2);
        let (out, b) = join_on_stack(
            NO_OWNER,
            || right.fill(2),
            |job| {
                left.fill(1);
                job.run_inline();
                7
            },
            |_| panic!("latch is set by run_inline"),
        );
        assert_eq!(out, 7);
        assert!(matches!(b, Some(Ok(()))));
        assert_eq!(data, [1, 1, 2, 2]);
    }

    #[test]
    fn thief_runs_on_other_thread_and_borrows_stack() {
        let value = String::from("borrowed");
        let (tx, rx) = mpsc::channel::<StackJob>();
        let thief = std::thread::spawn(move || {
            let job = rx.recv().unwrap();
            job.run()
        });
        let ((), b) = join_on_stack(
            3,
            || value.len(),
            |job| tx.send(job).unwrap(),
            |_| std::thread::yield_now(),
        );
        assert_eq!(b.and_then(Result::ok), Some(8));
        // Владелец не объявлял сон — будить некого.
        assert_eq!(thief.join().unwrap(), None);
    }

    #[test]
    fn sleepy_owner_is_reported_for_wakeup() {
        let (tx, rx) = mpsc::channel::<StackJob>();
        let (woken_tx, woken_rx) = mpsc::channel();
        let parked = &AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(move || {
                let job = rx.recv().unwrap();
                while !parked.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                woken_tx.send(job.run()).unwrap();
            });
            let ((), b) = join_on_stack(
                5,
                || 1,
                |job| tx.send(job).unwrap(),
                |latch| {
                    if latch.prepare_park() {
                        parked.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    std::thread::yield_now();
                },
            );
            assert!(matches!(b, Some(Ok(1))));
        });
        assert_eq!(woken_rx.recv().unwrap(), Some(5));
    }

    #[test]
    fn dropped_job_is_cancelled() {
        let ((), b) = join_on_stack(NO_OWNER, || 1, drop, |_| {});
        assert!(b.is_none());
    }

    #[test]
    fn panic_in_b_is_returned() {
        let ((), b) = join_on_stack(
            NO_OWNER,
            || -> u32 { panic!("b failed") },
            StackJob::run_inline,
            |_| {},
        );
        assert!(matches!(b, Some(Err(_))));
    }

    #[test]
    fn panic_in_body_waits_for_b_then_propagates() {
        let ran = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel::<StackJob>();
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            join_on_stack(
                NO_OWNER,
                || ran.store(true, std::sync::atomic::Ordering::SeqCst),
                |job| {
                    tx.send(job).unwrap();
                    panic!("a failed");
                },
                |_| {
                    if let Ok(job) = rx.try_recv() {
                        let _ = job.run();
                    }
                },
            )
        }));
        assert!(result.is_err());
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
    }
}
