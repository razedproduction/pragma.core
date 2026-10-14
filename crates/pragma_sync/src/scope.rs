use std::cell::UnsafeCell;
use std::fmt;
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};

use crate::AtomicCounter;
use crate::primitives::Arc;
use crate::primitives::{AtomicBool, Ordering};

type ErasedFn = Box<dyn FnOnce() + Send + 'static>;

/// A scope inside which tasks may borrow data from the stack (no `'static` needed).
///
/// Created only by the [`scope`] function, which does not return (neither normally nor by panic)
/// until every task created via [`task`](Self::task) or [`task_in`](Self::task_in) has finished
/// or been dropped. This is exactly what makes erasing the lifetime in [`ScopedTask`] sound.
///
/// `D` is scheduler data available to the tasks by reference for the whole scope
/// (for example a reference to the slot for the first error). `TaskScope` is covariant in `D`, so
/// `D` may borrow the caller's local data: `&'scope TaskScope<'scope, 'env, X<'a>>`
/// narrows to `&'scope TaskScope<'scope, 'env, X<'scope>>`.
pub struct TaskScope<'scope, 'env: 'scope, D> {
    pending: Arc<AtomicCounter>,
    data: D,
    // Invariant in both lifetimes, like `std::thread::Scope`. `'env` is needed for the
    // implied bound `'env: 'scope` in the `for<'scope>` closure `body`: without it the compiler
    // would demand `'static` from everything the tasks capture.
    scope: PhantomData<&'scope mut &'scope ()>,
    env: PhantomData<&'env mut &'env ()>,
}

/// A slot for a task on the caller's stack: a task from a slot does not allocate (see
/// [`TaskScope::task_in`]). For hot paths like `join`, where a `Box` per task
/// costs more than the work itself.
pub struct StackSlot<F> {
    /// The slot has been handed to a task: exactly one [`ScopedTask`] gets access to `f`.
    claimed: AtomicBool,
    f: UnsafeCell<Option<F>>,
}

// SAFETY: `f` is read and written only through the `ScopedTask` the slot was handed to (`claimed`
// guarantees there is only one such task); the slot's owner has no access to `f`. The closure
// moves to another thread, hence `F: Send` is required.
unsafe impl<F: Send> Sync for StackSlot<F> {}

impl<F> StackSlot<F> {
    /// A slot holding the closure `f`.
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

/// A scope task with its lifetime erased: it can go into any `'static` queue.
///
/// Run via [`run`](Self::run); dropping it without running also counts as
/// completion, so the scope will not hang on cancelled tasks. Leaking it via
/// `mem::forget` is safe but hangs the scope's wait forever.
pub struct ScopedTask {
    body: TaskBody,
    // Declared after `body`: when the task is dropped, the captured values are destroyed before
    // the scope learns about the completion, even if a captured value's destructor panics.
    // The field is never read: all the work is in `Drop`.
    _done: DoneGuard,
}

enum TaskBody {
    Boxed(Option<ErasedFn>),
    /// The closure lives in a [`StackSlot`] on the stack of the scope's owner.
    Slot {
        slot: *const (),
        /// The monomorphized `call_slot::<F>`.
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
            // SAFETY: `slot` was created in `task_in` from a `&'scope StackSlot<F>` with `call_slot::<F>`;
            // the slot is alive until the task has finished (the scope waits for it), and the task is the
            // sole holder of access to it. `pending` guarantees a single call.
            unsafe { call(*slot, false) };
        }
    }
}

/// Takes the closure out of the slot and runs (`run`) or destroys it.
///
/// # Safety
/// `ptr` points to a live `StackSlot<F>` whose access belongs to the calling task.
unsafe fn call_slot<F: FnOnce()>(ptr: *const (), run: bool) {
    // SAFETY: by the function's contract `ptr` points to a live `StackSlot<F>`.
    let slot = unsafe { &*ptr.cast::<StackSlot<F>>() };
    // SAFETY: `f` is accessed only by the task the slot was handed to (`claimed`), and only here;
    // there are no other references to the cell's contents.
    let f = unsafe { (*slot.f.get()).take() };
    if run {
        if let Some(f) = f {
            f();
        }
    } else {
        drop(f);
    }
}

// SAFETY: `Boxed` holds a `Send` closure. `Slot` points to a `StackSlot<F>` with `F: Send`
// (required by `task_in`), which is `Sync` and lives until the task has finished; moving the task
// to another thread hands over only exclusive access to that slot.
unsafe impl Send for ScopedTask {}

struct DoneGuard(Arc<AtomicCounter>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.0.decrement();
    }
}

impl ScopedTask {
    /// Runs the task. A panic of the closure propagates to the caller, but the scope still
    /// receives the completion signal.
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
                    // Cleared before the call: on panic, `Drop` will not touch the slot again.
                    *pending = false;
                    // SAFETY: see `TaskBody::drop`: the same invariants, a single call.
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
    /// The scheduler data passed to [`scope`].
    #[must_use]
    pub fn data(&self) -> &D {
        &self.data
    }

    /// How many of the scope's tasks have not finished yet.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.get()
    }

    fn register(&self) -> DoneGuard {
        if self.pending.increment().is_err() {
            // Unreachable: every live task holds a clone of the counter's `Arc`, and `Arc`
            // aborts the process when its reference count overflows (isize::MAX) before this
            // counter can reach usize::MAX. Without the increment the lifetime must not be
            // erased, so this is an abort here too rather than silently carrying on.
            std::process::abort();
        }
        DoneGuard(Arc::clone(&self.pending))
    }

    /// Wraps a closure into a scope task; the scope will wait for it to finish or be dropped.
    pub fn task<F>(&self, f: F) -> ScopedTask
    where
        F: FnOnce() + Send + 'scope,
    {
        let done = self.register();
        let boxed: Box<dyn FnOnce() + Send + 'scope> = Box::new(f);
        // SAFETY: only the trait object's lifetime changes; the layout of `Box<dyn ...>` is the same.
        // The closure lives no longer than `'scope`: the `pending` counter was incremented above and is
        // decremented only in `DoneGuard::drop`, after the closure has run or been destroyed.
        // `scope()` does not return (neither normally nor by panic) while `pending` is non-zero,
        // and if the wait is interrupted by a panic, it aborts the process.
        // `'scope` is a borrow of the local `TaskScope` inside `scope()`, so it ends before
        // the return, and everything captured with a lifetime `>= 'scope` is still alive.
        let erased: ErasedFn =
            unsafe { std::mem::transmute::<Box<dyn FnOnce() + Send + 'scope>, ErasedFn>(boxed) };
        ScopedTask {
            body: TaskBody::Boxed(Some(erased)),
            _done: done,
        }
    }

    /// A task from a slot on the stack, with no allocation. The slot must outlive the scope (`&'scope`);
    /// each slot is handed to only one task, so a repeated call returns `None`.
    pub fn task_in<F>(&self, slot: &'scope StackSlot<F>) -> Option<ScopedTask>
    where
        F: FnOnce() + Send + 'scope,
    {
        if slot.claimed.swap(true, Ordering::AcqRel) {
            return None;
        }
        let done = self.register();
        // The pointer lives as long as the `&'scope StackSlot<F>` borrow: the slot
        // outlives the scope, and the scope waits for the task to finish (see SAFETY in `task`).
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

/// Counters of finished scopes, ready for reuse: an allocation per
/// `join` would cost more than the `join` itself.
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

    /// Called while the scope still owns the counter.
    pub(super) fn give_back(counter: &Arc<AtomicCounter>) {
        // Zero means every task of the scope has already made its single `decrement`.
        // `Arc` clones may still live in tasks in the tail of `decrement` (after reaching zero): there
        // `waiters` is read and waiters may be woken. The tail does not change the count, and a spurious
        // wakeup of the next scope's waiters is harmless: they recheck the value.
        // Requiring `strong_count == 1` is not an option: this race is frequent, and each lost one
        // would cost an allocation of a new counter in the next scope (the graph frame would stop
        // being allocation-free).
        if !counter.is_zero() {
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

/// Runs a scope of tasks with borrows.
///
/// `body` creates tasks via [`TaskScope::task`] and hands them to an executor. After `body`
/// the function waits for all tasks to finish, calling `help` (help-while-wait, see
/// [`AtomicCounter::wait_while_helping`]), and returns the result of `body`. To read the
/// tasks' results after the scope (for example collected errors), put a reference to your
/// local data into `data`.
///
/// A panic in `body` is propagated after all tasks have finished. A panic in `help`
/// leads to `abort`: unwinding the stack cannot continue while tasks hold references into it.
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

    /// The simplest executor: tasks go into a channel and other threads run them.
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
        // The slot holds `&mut out` until the end of its life.
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
