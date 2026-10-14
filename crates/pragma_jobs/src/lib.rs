//! L2: work-stealing job system `pragma.core`.
//!
//! Пул рабочих потоков ([`ThreadPool`]) с локальными LIFO-очередями, глобальными очередями
//! по приоритетам и кражей задач (`docs/architecture.md` §3.1–3.2). Любое ожидание
//! (`join`, `scope`, [`JobHandle::join`]) исполняет чужие задачи, пока ждёт своих
//! (help-while-wait), поэтому вложенный параллелизм не дедлочит даже на одном воркере.
//! Паника в задаче перехватывается и возвращается как [`JobError::Panicked`].
//!
//! ```
//! use pragma_jobs::{Grain, PoolConfig, Priority, ThreadPool};
//!
//! let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
//!
//! // fork-join
//! let (a, b) = pool.join(|| 1 + 1, || 2 + 2)?;
//! assert_eq!((a, b), (2, 4));
//!
//! // задачи заимствуют данные со стека
//! let mut data = vec![1_u32; 1000];
//! pool.scope(|s| {
//!     for chunk in data.chunks_mut(100) {
//!         s.spawn(move || chunk.iter_mut().for_each(|x| *x *= 2));
//!     }
//! })?;
//! assert!(data.iter().all(|&x| x == 2));
//!
//! pool.parallel_for(&mut data, Grain::Auto, |x| *x += 1)?;
//! let sum = pool.parallel_reduce(&data, Grain::Fixed(64), |_, c| c.iter().sum::<u32>(), |a, b| a + b)?;
//! assert_eq!(sum, Some(3000));
//!
//! // 'static-задача с хэндлом; паника не роняет процесс
//! let handle = pool.spawn(Priority::Background, || -> u32 { panic!("bake failed") });
//! assert!(handle.join().is_err());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Наблюдаемость (`docs/architecture.md` §5)
//!
//! - [`ThreadPool::stats`] — метрики пула ([`PoolStats`]) без остановки воркеров.
//! - [`current_task`] — метка задачи, исполняемой на этом потоке (для panic hook и
//!   крэш-репортов); метки ставит [`labeled`]. Метки получают задачи графа (`graph`),
//!   [`ThreadPool::spawn`] (`spawn`), задачи [`Scope::spawn`] и `parallel_*` (`scope`),
//!   [`Spawner::spawn_detached`] (`detached`) и async-задачи `pragma_async` (`async`,
//!   `unblock`). Вторая ветвь `join` метки не получает: это горячий путь, а её имя
//!   ничего бы не сказало — виновника назовёт внешняя метка.
//! - Watchdog зависших задач в debug-сборках ([`PoolConfig::with_watchdog`]).
//! - `tracing`: span `scope`/`parallel_for`/`parallel_reduce` на уровне `TRACE`; уровни
//!   отключаются при компиляции фичами `tracing` (`max_level_*`, `release_max_level_*`).
//! - Feature `puffin`: те же области — в профайлере кадра `puffin`.
//! - [`sys`]: имена, приоритеты потоков и привязка к ядрам (крейт `pragma_sys`).
#![forbid(unsafe_code)]

mod config;
mod error;
mod handle;
mod pool;
mod priority;
mod scope;
mod spawner;
mod stats;
#[cfg(debug_assertions)]
mod watchdog;

pub use config::{DEFAULT_IDLE_SPIN, DEFAULT_WATCHDOG, PoolConfig, ShutdownPolicy};
pub use error::{JobError, JobsError};
pub use handle::JobHandle;
pub use pool::ThreadPool;
pub use pragma_sync::{TaskLabel, current_label as current_task};
pub use priority::{Grain, Priority};
pub use scope::Scope;
pub use spawner::Spawner;
pub use stats::{PoolStats, ThreadStats};

/// Платформенный слой: имена, приоритеты потоков и привязка к ядрам (re-export
/// `pragma_sys`; `unsafe` FFI живёт там, DECISIONS #27).
pub use pragma_sys as sys;

/// Исполняет `f` с меткой задачи `kind`/`name` на текущем потоке.
///
/// Метку видит [`current_task`] (в том числе из panic hook) — так приложение называет
/// виновника падения. В debug-сборках задача на потоке пула ещё и отмечается для watchdog
/// ([`PoolConfig::with_watchdog`]). В release — две записи в TLS, без аллокаций.
///
/// ```
/// use pragma_jobs::{current_task, labeled};
///
/// let name = labeled("asset", "level.bin", || current_task(|t| t.map(|t| t.name().to_owned())));
/// assert_eq!(name.as_deref(), Some("level.bin"));
/// ```
///
/// Panic hook, который называет задачу (без аллокаций до форматирования отчёта):
///
/// ```no_run
/// std::panic::set_hook(Box::new(|info| {
///     pragma_jobs::current_task(|task| match task {
///         Some(task) => eprintln!("panic in {task}: {info}"),
///         None => eprintln!("panic outside pool tasks: {info}"),
///     });
/// }));
/// ```
pub fn labeled<R>(kind: &'static str, name: &str, f: impl FnOnce() -> R) -> R {
    #[cfg(debug_assertions)]
    let _watch = pool::watch(kind, name);
    pragma_sync::run_labeled(kind, name, f)
}

/// Приоритет задачи пула, которую исполняет текущий поток; `None` — поток сейчас не
/// исполняет задачу пула (например, главный поток в теле `scope` или вне пула).
#[must_use]
pub fn current_priority() -> Option<Priority> {
    pool::current_priority()
}
