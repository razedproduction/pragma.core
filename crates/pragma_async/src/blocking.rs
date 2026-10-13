use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::{self, JoinHandle};

use async_task::Runnable;
use pragma_base::tracing;

use crate::{AsyncConfig, AsyncError};

/// Пул потоков для синхронного (блокирующего) I/O — DECISIONS #23.
///
/// Потоки создаются по требованию до `blocking_threads` и живут до сброса владельца.
/// Задачи — `Runnable` из `async-task`: отмена хэндла до старта превращает задачу в сброс
/// замыкания, а результат и пробуждение ожидающего даёт сам `async-task`.
pub(crate) struct BlockingPool {
    shared: Arc<BlockingShared>,
}

struct BlockingShared {
    state: Mutex<BlockingState>,
    work: Condvar,
    /// Поставленные и ещё не завершённые задачи (в очереди и в работе).
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
            // Пул сброшен: `Runnable` сбрасывается — задача отменяется.
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

    /// Гарантирует, что поставленную сейчас задачу кто-то возьмёт: свободный поток или
    /// новый, если лимит не исчерпан.
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
                // Задачу возьмёт один из уже запущенных потоков, когда освободится.
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
            // Паника замыкания перехвачена внутри future; сюда может долететь только паника
            // деструктора — поток всё равно продолжает работу.
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

/// Слот в лимите `blocking_capacity`; освобождается, когда задача завершена или сброшена.
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

    /// Занимает слот в лимите поставленных задач или сразу возвращает `Overloaded`.
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

    /// Ставит задачу: `spawn` получает функцию планирования и возвращает `Runnable`
    /// с хэндлом (`async_task::spawn`).
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

    /// Число запущенных потоков (для тестов лимита).
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
        // Невзятые задачи отменяются (их хэндлы получают `Cancelled`). Сброс — вне блокировки:
        // деструкторы будят ожидающих, а те могут снова планировать задачи.
        for runnable in queued {
            if panic::catch_unwind(AssertUnwindSafe(|| drop(runnable))).is_err() {
                tracing::error!("panic while dropping a cancelled blocking task");
            }
        }
        // Задача, которая уже исполняется, доработает: синхронный I/O не прервать.
        let me = thread::current().id();
        for handle in threads {
            if handle.thread().id() == me {
                // Исполнитель сброшен из собственной blocking-задачи: поток выйдет сам.
                continue;
            }
            if handle.join().is_err() {
                tracing::error!("blocking thread terminated with a panic");
            }
        }
    }
}
