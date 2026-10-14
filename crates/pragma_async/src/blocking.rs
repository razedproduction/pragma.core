use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::{self, JoinHandle};

use async_task::Runnable;

use crate::{AsyncConfig, AsyncError};

/// A thread pool for synchronous (blocking) I/O, DECISIONS #23.
///
/// Threads are created on demand up to `blocking_threads` and live until the owner is dropped.
/// Tasks are `async-task` `Runnable`s: cancelling the handle before the start turns the task into
/// dropping the closure, and `async-task` itself delivers the result and wakes the waiter.
pub(crate) struct BlockingPool {
    shared: Arc<BlockingShared>,
}

struct BlockingShared {
    state: Mutex<BlockingState>,
    work: Condvar,
    /// Queued tasks that have not finished yet (waiting and running).
    pending: AtomicUsize,
    config: AsyncConfig,
}

#[derive(Default)]
struct BlockingState {
    queue: VecDeque<Runnable>,
    threads: Vec<JoinHandle<()>>,
    idle: usize,
    shutdown: bool,
}

impl BlockingShared {
    fn lock(&self) -> MutexGuard<'_, BlockingState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn schedule(weak: &Weak<Self>, runnable: Runnable) {
        let Some(shared) = weak.upgrade() else {
            // The pool is gone: the `Runnable` is dropped, which cancels the task.
            return;
        };
        let mut state = shared.lock();
        if state.shutdown {
            drop(state);
            drop(runnable);
            return;
        }
        state.queue.push_back(runnable);
        drop(state);
        shared.work.notify_one();
    }

    /// Guarantees that the task being queued now will be taken by someone: an idle thread or
    /// a new one, if the limit is not exhausted.
    fn ensure_thread(self: &Arc<Self>) -> Result<(), AsyncError> {
        let mut state = self.lock();
        if state.shutdown {
            return Err(AsyncError::Cancelled);
        }
        if state.idle > state.queue.len() || state.threads.len() >= self.config.blocking_threads() {
            return Ok(());
        }
        let name = format!("{}-{}", self.config.name_prefix(), state.threads.len());
        let mut builder = thread::Builder::new().name(name.clone());
        if let Some(bytes) = self.config.stack_size() {
            builder = builder.stack_size(bytes);
        }
        let shared = Arc::clone(self);
        match builder.spawn(move || worker_main(&shared)) {
            Ok(handle) => {
                state.threads.push(handle);
                Ok(())
            }
            Err(error) if state.threads.is_empty() => Err(AsyncError::SpawnFailed {
                thread: name,
                message: error.to_string(),
            }),
            Err(error) => {
                // One of the threads already running will take the task when it frees up.
                tracing::warn!(thread = %name, %error, "failed to spawn extra blocking thread");
                Ok(())
            }
        }
    }
}

fn worker_main(shared: &BlockingShared) {
    let mut state = shared.lock();
    loop {
        if let Some(runnable) = state.queue.pop_front() {
            drop(state);
            // A panic of the closure is caught inside the future; only a destructor panic can
            // reach here, and the thread keeps running anyway.
            if panic::catch_unwind(AssertUnwindSafe(|| runnable.run())).is_err() {
                tracing::error!("panic escaped a blocking task; thread keeps running");
            }
            state = shared.lock();
            continue;
        }
        if state.shutdown {
            break;
        }
        state.idle = state.idle.saturating_add(1);
        state = shared
            .work
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner);
        state.idle = state.idle.saturating_sub(1);
    }
}

/// A slot within the `blocking_capacity` limit; released when the task finishes or is dropped.
pub(crate) struct PendingSlot {
    shared: Weak<BlockingShared>,
}

impl Drop for PendingSlot {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl BlockingPool {
    pub(crate) fn new(config: AsyncConfig) -> Self {
        Self {
            shared: Arc::new(BlockingShared {
                state: Mutex::new(BlockingState::default()),
                work: Condvar::new(),
                pending: AtomicUsize::new(0),
                config,
            }),
        }
    }

    /// Takes a slot within the queued task limit or immediately returns `Overloaded`.
    pub(crate) fn reserve(&self) -> Result<PendingSlot, AsyncError> {
        let capacity = self.shared.config.blocking_capacity();
        let pending = &self.shared.pending;
        let mut current = pending.load(Ordering::Acquire);
        loop {
            if current >= capacity {
                return Err(AsyncError::Overloaded { capacity });
            }
            match pending.compare_exchange_weak(
                current,
                current.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(PendingSlot {
                        shared: Arc::downgrade(&self.shared),
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Queues a task: `spawn` receives the schedule function and returns a `Runnable`
    /// with its handle (`async_task::spawn`).
    pub(crate) fn submit<H>(
        &self,
        spawn: impl FnOnce(Box<dyn Fn(Runnable) + Send + Sync>) -> (Runnable, H),
    ) -> Result<H, AsyncError> {
        self.shared.ensure_thread()?;
        let weak = Arc::downgrade(&self.shared);
        let (runnable, handle) = spawn(Box::new(move |r| BlockingShared::schedule(&weak, r)));
        runnable.schedule();
        Ok(handle)
    }

    /// Number of running threads (for limit tests).
    pub(crate) fn threads(&self) -> usize {
        self.shared.lock().threads.len()
    }
}

impl Drop for BlockingPool {
    fn drop(&mut self) {
        let (queued, threads) = {
            let mut state = self.shared.lock();
            state.shutdown = true;
            (
                std::mem::take(&mut state.queue),
                std::mem::take(&mut state.threads),
            )
        };
        self.shared.work.notify_all();
        // Tasks not yet taken are cancelled (their handles get `Cancelled`). Dropping happens outside
        // the lock: destructors wake waiters, and those may schedule tasks again.
        for runnable in queued {
            if panic::catch_unwind(AssertUnwindSafe(|| drop(runnable))).is_err() {
                tracing::error!("panic while dropping a cancelled blocking task");
            }
        }
        // A task that is already running will finish: synchronous I/O cannot be interrupted.
        let me = thread::current().id();
        for handle in threads {
            if handle.thread().id() == me {
                // The executor is dropped from its own blocking task: the thread will exit by itself.
                continue;
            }
            if handle.join().is_err() {
                tracing::error!("blocking thread terminated with a panic");
            }
        }
    }
}
