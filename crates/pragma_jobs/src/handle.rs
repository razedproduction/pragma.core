use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, PoisonError};

use pragma_sync::AtomicCounter;

use crate::pool::{Job, Shared, enter, help_once};
use crate::{JobError, Priority, ThreadPool};

const SPAWN_JOB: &str = "spawn";

struct JobState<T> {
    done: AtomicCounter,
    result: Mutex<Option<Result<T, JobError>>>,
}

/// Хэндл задачи, запущенной через [`ThreadPool::spawn`].
///
/// Сброс хэндла не отменяет задачу: она доработает, результат будет выброшен.
#[must_use = "без join() результат и паника задачи теряются"]
pub struct JobHandle<T> {
    state: Arc<JobState<T>>,
    shared: Arc<Shared>,
}

impl<T> std::fmt::Debug for JobHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobHandle")
            .field("finished", &self.is_finished())
            .finish()
    }
}

impl<T> JobHandle<T> {
    /// Задача завершилась (успешно, паникой или отменой).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state.done.is_zero()
    }

    /// Ждёт результат. Пока ждёт, исполняет другие задачи пула (help-while-wait), поэтому
    /// вызов из задачи того же пула не дедлочит даже на одном воркере.
    pub fn join(self) -> Result<T, JobError> {
        let shared = &self.shared;
        enter(shared, || {
            self.state.done.wait_while_helping(|| help_once(shared));
        });
        self.state
            .result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| Err(JobError::cancelled(SPAWN_JOB)))
    }
}

/// Сторона исполнителя: записывает результат; сброс без результата означает отмену.
struct Completer<T> {
    state: Arc<JobState<T>>,
}

impl<T> Completer<T> {
    fn complete(self, result: Result<T, JobError>) {
        *self
            .state
            .result
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(result);
        // Сигнал о завершении — в `Drop`.
    }
}

impl<T> Drop for Completer<T> {
    fn drop(&mut self) {
        {
            let mut slot = self
                .state
                .result
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(Err(JobError::cancelled(SPAWN_JOB)));
            }
        }
        self.state.done.decrement();
    }
}

impl ThreadPool {
    /// Запускает `'static`-задачу с приоритетом `priority` и возвращает хэндл на результат.
    pub fn spawn<F, T>(&self, priority: Priority, f: F) -> JobHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let state = Arc::new(JobState {
            done: AtomicCounter::new(1),
            result: Mutex::new(None),
        });
        let completer = Completer {
            state: Arc::clone(&state),
        };
        let job = Job::Boxed(Box::new(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(f))
                .map_err(|payload| JobError::panicked(SPAWN_JOB, &*payload));
            completer.complete(result);
        }));
        self.shared.push(job, priority);
        JobHandle {
            state,
            shared: Arc::clone(&self.shared),
        }
    }
}
