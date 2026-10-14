use std::sync::{Arc, Weak};

use crate::pool::{Job, Shared};
use crate::{JobError, Priority, ThreadPool};

const DETACHED_JOB: &str = "spawn_detached";

/// Лёгкая ссылка на пул для задач без хэндла: основа исполнителей поверх пула
/// (`pragma_async` планирует через неё `Runnable` своих future).
///
/// Клонируется дёшево и **не держит пул живым**: после начала остановки пула
/// ([`ThreadPool`] сброшен) задачи не принимаются и сбрасываются без выполнения.
///
/// ```
/// use std::sync::mpsc;
/// use pragma_jobs::{PoolConfig, Priority, ThreadPool};
///
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(1))?;
/// let spawner = pool.spawner();
/// let (tx, rx) = mpsc::channel();
/// spawner.spawn_detached(Priority::Normal, move || tx.send(7).unwrap_or(()))?;
/// assert_eq!(rx.recv()?, 7);
///
/// drop(pool);
/// assert!(spawner.spawn_detached(Priority::Normal, || ()).is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct Spawner {
    shared: Weak<Shared>,
}

impl std::fmt::Debug for Spawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spawner")
            .field("alive", &self.is_alive())
            .finish()
    }
}

impl Spawner {
    /// Ставит `'static`-задачу в очередь приоритета `priority` без хэндла на результат.
    ///
    /// Паника задачи перехватывается воркером и пишется в лог, воркер продолжает работу;
    /// если результат или паника важны вызывающему, задача сообщает их сама.
    ///
    /// Если пул уже останавливается или остановлен, задача сбрасывается **без выполнения**
    /// (её деструктор вызывается сразу) и возвращается [`JobError::Cancelled`]. Задача,
    /// принятая в гонке с остановкой, тоже может быть сброшена без выполнения — так же, как
    /// задачи очередей при [`ShutdownPolicy::Cancel`](crate::ShutdownPolicy::Cancel).
    pub fn spawn_detached<F>(&self, priority: Priority, f: F) -> Result<(), JobError>
    where
        F: FnOnce() + Send + 'static,
    {
        let Some(shared) = self.shared.upgrade() else {
            drop(f);
            return Err(JobError::cancelled(DETACHED_JOB));
        };
        let job = Job::Boxed(Box::new(move || {
            crate::labeled("detached", DETACHED_JOB, f)
        }));
        if shared.push_detached(job, priority) {
            Ok(())
        } else {
            Err(JobError::cancelled(DETACHED_JOB))
        }
    }

    /// Пул ещё принимает задачи (не начал остановку).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.shared.upgrade().is_some_and(|s| !s.is_shutdown())
    }

    /// Текущий поток исполняет работу этого пула: это его воркер или внешний поток внутри
    /// `scope`/`join`/[`JobHandle::join`](crate::JobHandle::join) (например, главный поток
    /// во время кадра графа). Блокирующее ожидание здесь задержало бы кадр или повесило пул.
    #[must_use]
    pub fn in_pool_context(&self) -> bool {
        self.shared.upgrade().is_some_and(|s| s.in_context())
    }
}

impl ThreadPool {
    /// Лёгкая ссылка на пул для задач без хэндла (см. [`Spawner`]).
    #[must_use]
    pub fn spawner(&self) -> Spawner {
        Spawner {
            shared: Arc::downgrade(&self.shared),
        }
    }
}
