use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;

use async_task::Runnable;
use futures_lite::FutureExt;
use pragma_jobs::{Priority, Spawner, ThreadPool};

use crate::blocking::BlockingPool;
use crate::{AsyncConfig, AsyncError, AsyncHandle};

const SPAWN_ASYNC: &str = "spawn_async";
const UNBLOCK: &str = "unblock";

/// Исполнитель async-задач поверх [`ThreadPool`] (`docs/architecture.md` §3.4).
///
/// - [`spawn_async`](Self::spawn_async): future исполняется на воркерах пула с приоритетом
///   [`Priority::Background`] — в пределах лимита фоновых воркеров, не вытесняя кадр.
/// - [`unblock`](Self::unblock): синхронное (блокирующее) замыкание, например чтение файла,
///   исполняется в отдельном ограниченном blocking-пуле и не занимает воркеры пула.
///
/// Клонируется дёшево (`Arc`), клон можно захватить в future, чтобы запускать подзадачи.
/// Исполнитель не держит пул живым: после остановки пула незавершённые задачи отменяются
/// ([`AsyncError::Cancelled`]). Сброс последнего клона отменяет невзятые blocking-задачи и
/// дожидается выполняющихся (синхронный I/O не прервать).
#[derive(Clone)]
pub struct Executor {
    inner: Arc<Inner>,
}

struct Inner {
    spawner: Spawner,
    blocking: BlockingPool,
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("pool", &self.inner.spawner)
            .field("blocking_threads", &self.inner.blocking.threads())
            .finish()
    }
}

impl Executor {
    /// Создаёт исполнитель над `pool`. Потоки blocking-пула создаются по требованию.
    pub fn new(pool: &ThreadPool, config: AsyncConfig) -> Result<Self, AsyncError> {
        let config = config.validate()?;
        Ok(Self {
            inner: Arc::new(Inner {
                spawner: pool.spawner(),
                blocking: BlockingPool::new(config),
            }),
        })
    }

    /// Запускает future на воркерах пула с приоритетом [`Priority::Background`].
    ///
    /// Паника в future перехватывается и возвращается как [`AsyncError::Panicked`]; воркер
    /// продолжает работу. Сброс хэндла отменяет задачу. Если пул остановлен, хэндл вернёт
    /// [`AsyncError::Cancelled`].
    pub fn spawn_async<F>(&self, future: F) -> AsyncHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let caught = AssertUnwindSafe(future).catch_unwind();
        let future = async move {
            caught
                .await
                .map_err(|payload| AsyncError::panicked(SPAWN_ASYNC, &*payload))
        };
        let spawner = self.inner.spawner.clone();
        let schedule = move |runnable: Runnable| {
            // Отказ (пул остановлен) сбрасывает `Runnable` — `async-task` отменяет задачу и
            // будит ожидающего, поэтому ошибку здесь обрабатывать не нужно.
            let _ = spawner.spawn_detached(Priority::Background, move || {
                runnable.run();
            });
        };
        let (runnable, task) = async_task::spawn(future, schedule);
        runnable.schedule();
        AsyncHandle::running(task.fallible(), self.inner.spawner.clone())
    }

    /// Исполняет синхронное замыкание в blocking-пуле (файловый I/O, декомпрессия).
    ///
    /// Вызов не блокирует: если поставлено уже `blocking_capacity` задач, хэндл сразу
    /// содержит [`AsyncError::Overloaded`]. Работает и из future (`.await` на хэндле), и
    /// из кадра (`poll_ready`). Сброс хэндла до старта отменяет задачу; начатая —
    /// доработает, результат будет сброшен. Паника — [`AsyncError::Panicked`].
    pub fn unblock<F, T>(&self, f: F) -> AsyncHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let spawner = self.inner.spawner.clone();
        let slot = match self.inner.blocking.reserve() {
            Ok(slot) => slot,
            Err(error) => return AsyncHandle::ready(Err(error), spawner),
        };
        let future = async move {
            let _slot = slot;
            panic::catch_unwind(AssertUnwindSafe(f))
                .map_err(|payload| AsyncError::panicked(UNBLOCK, &*payload))
        };
        match self
            .inner
            .blocking
            .submit(|schedule| async_task::spawn(future, schedule))
        {
            Ok(task) => AsyncHandle::running(task.fallible(), spawner),
            Err(error) => AsyncHandle::ready(Err(error), spawner),
        }
    }

    /// Пул, на котором исполняются задачи, ещё принимает работу.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.inner.spawner.is_alive()
    }

    /// Сколько потоков blocking-пула запущено сейчас (не больше `blocking_threads`).
    #[must_use]
    pub fn blocking_threads(&self) -> usize {
        self.inner.blocking.threads()
    }
}
