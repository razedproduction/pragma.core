//! Fork-join without allocations and without `Arc`: the second branch of `join` is a frame on the
//! owner's stack with a latch (DECISIONS #30).
//!
//! The owner pushes a [`StackJob`] into its queue, runs the first branch and takes the job
//! back. A job that was not stolen the owner runs itself ([`StackJob::run_inline`]), with no
//! atomic RMW at all. A stolen job is run by the thief ([`StackJob::run`]): it writes the result and
//! sets the latch with a single `swap`; if by then the owner has decided to sleep
//! ([`JoinLatch::prepare_park`]), `run` returns the owner's index and the caller must wake it.
//! After the `swap` the thief does not touch the frame's memory: the owner may return right away.

use std::cell::UnsafeCell;
use std::panic::{self, AssertUnwindSafe};
use std::{fmt, ptr, thread};

use crate::primitives::{AtomicUsize, Ordering};

const PENDING: usize = 0;
/// The owner is about to sleep: the executor must wake it.
const SLEEPY: usize = 1;
const SET: usize = 2;

/// Index of an owner that nobody can wake (it has no parker): `run` never
/// returns it, and the owner rechecks the latch by itself.
pub const NO_OWNER: usize = usize::MAX;

/// The frame of the second branch on the stack of [`join_on_stack`].
struct Frame<F, R> {
    latch: AtomicUsize,
    owner: usize,
    /// Read and written only by the job's executor (there is exactly one: the job is not cloned).
    f: UnsafeCell<Option<F>>,
    /// Written by the executor before the latch is set, read by the owner after.
    result: UnsafeCell<Option<thread::Result<R>>>,
}

#[derive(Clone, Copy)]
enum Mode {
    /// Run by the owner: the latch is set with a plain store.
    Owner,
    /// Run by another thread.
    Thief,
    /// The job is dropped without running.
    Cancel,
}

/// The second branch of `join` with its type and lifetime erased, so it can go into a `'static` queue.
///
/// It is run via [`run`](Self::run) or [`run_inline`](Self::run_inline); dropping it without
/// running also sets the latch, so the owner will not hang. Leaking it via `mem::forget`
/// is safe but hangs the owner forever.
pub struct StackJob {
    frame: *const (),
    /// The monomorphized `call::<F, R>`.
    call: unsafe fn(*const (), Mode) -> Option<usize>,
    pending: bool,
}

// SAFETY: the frame requires `F: Send` and `R: Send` (see `join_on_stack`); moving the job to another
// thread hands over the sole access to the closure and the result slot, and the latch is an atomic.
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
    /// The job identifier (the frame address): the owner recognizes its job in the queue by it.
    #[must_use]
    pub fn id(&self) -> usize {
        self.frame.addr()
    }

    /// Runs the job on a foreign thread. A panic of the closure is caught and handed to the
    /// owner as the result. Returns the owner's index if it has to be woken.
    #[must_use = "an owner that is about to sleep must be woken"]
    pub fn run(mut self) -> Option<usize> {
        self.pending = false;
        // SAFETY: `frame` and `call` were created in `join_on_stack` for the same frame; the frame is
        // alive until the latch is set (the owner waits for it), and `pending` guarantees a single call.
        unsafe { (self.call)(self.frame, Mode::Thief) }
    }

    /// Runs the job on the owner's thread (the one that called [`join_on_stack`]) with no atomic
    /// RMW. Correct from another thread too, but it will not wake a sleeping owner (that one wakes
    /// up on its parking timeout).
    pub fn run_inline(mut self) {
        self.pending = false;
        // SAFETY: see `run`.
        let _ = unsafe { (self.call)(self.frame, Mode::Owner) };
    }
}

impl Drop for StackJob {
    fn drop(&mut self) {
        if self.pending {
            self.pending = false;
            // SAFETY: see `run`. There is nothing to wake the owner with here: it wakes up on its timeout.
            let _ = unsafe { (self.call)(self.frame, Mode::Cancel) };
        }
    }
}

/// Runs (or drops) the frame's closure and sets the latch.
///
/// # Safety
/// `ptr` points to a live `Frame<F, R>`, and this is the first and only call for that frame.
unsafe fn call<F: FnOnce() -> R, R>(ptr: *const (), mode: Mode) -> Option<usize> {
    // SAFETY: by the function's contract.
    let frame = unsafe { &*ptr.cast::<Frame<F, R>>() };
    // Read before setting the latch: after that the frame may already be gone.
    let owner = frame.owner;
    // SAFETY: only this (single) call accesses `f`.
    let f = unsafe { (*frame.f.get()).take() };
    match (mode, f) {
        (Mode::Cancel, f) => {
            // The destructor of the captured state is user code: its panic must not
            // escape past the latch.
            let _ = panic::catch_unwind(AssertUnwindSafe(|| drop(f)));
        }
        (_, Some(f)) => {
            let result = panic::catch_unwind(AssertUnwindSafe(f));
            // SAFETY: the owner reads `result` only after it sees the latch (below);
            // there are no other writers.
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

/// The latch of the second branch, for the owner's wait step (see [`join_on_stack`]).
#[derive(Debug)]
pub struct JoinLatch<'a> {
    state: &'a AtomicUsize,
}

impl JoinLatch<'_> {
    /// Whether the second branch has finished (or was dropped).
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.state.load(Ordering::Acquire) == SET
    }

    /// Announces that the owner is about to sleep on its parker. `true`: it may park, and the
    /// executor will return the owner's index from [`StackJob::run`] so that it gets woken;
    /// `false`: the latch is already set, no need to sleep.
    ///
    /// A wakeup is never lost: both the announcement and setting the latch are RMWs on one atomic, so
    /// either the executor sees `SLEEPY` and wakes the owner, or the owner sees `SET` (loom model).
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

/// Fork-join: creates a job for `b` on the stack and passes it to `body`.
///
/// `body` pushes the job into a queue (it will be run by [`StackJob::run`] or, if the owner takes
/// it back, by [`StackJob::run_inline`]), runs the first branch and returns its result.
/// After `body` the function waits for the latch, calling `wait` (a help-while-wait step: run
/// someone else's task, spin, or sleep after [`JoinLatch::prepare_park`]), and returns the results of
/// `body` and `b`: `None` means the job was dropped without running, `Some(Err(_))` means `b` panicked.
///
/// A panic in `body` is propagated after the job has finished. A panic in `wait` leads
/// to `abort`: unwinding cannot continue while the job references the stack.
///
/// `owner` is the owner's index for wakeups (or [`NO_OWNER`]).
///
/// ```
/// use pragma_sync::__private::{NO_OWNER, join_on_stack};
///
/// let mut sum = 0_u64;
/// let ((), b) = join_on_stack(
///     NO_OWNER,
///     || (1..=10_u64).sum::<u64>(),
///     |job| {
///         sum = 100;          // first branch
///         job.run_inline();   // nobody stole it, so run it ourselves
///     },
///     |_latch| unreachable!("the latch is already set"),
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
    // SAFETY invariant (relied upon by `StackJob`): the frame does not move and lives until the end
    // of the function, and the function does not return (neither normally nor by panic) until the
    // latch is set; after that the executor does not touch the frame.
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
        // The owner never announced sleep, so there is nobody to wake.
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
