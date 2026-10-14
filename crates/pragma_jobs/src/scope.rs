use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, PoisonError};

use pragma_base::tracing;
use pragma_sync::{StackSlot, TaskScope};

use crate::pool::{Job, Shared, enter, help_once, inherited_priority};
use crate::{Grain, JobError, Priority, ThreadPool};

/// Span `tracing` (уровень `TRACE`) и область puffin на время операции пула `$op`.
/// Поля span вычисляются, только если уровень включён.
macro_rules! observe {
    ($pool:expr, $op:literal) => {
        let _span = tracing::trace_span!(
            $op,
            priority = inherited_priority().as_str(),
            worker = $pool.current_worker_index(),
        )
        .entered();
        #[cfg(feature = "puffin")]
        puffin::profile_scope!($op);
    };
}

/// Данные области: всё заимствовано у `scope_named`, поэтому область не аллоцирует и не
/// трогает общий для всех потоков счётчик ссылок `Arc<Shared>`.
#[derive(Clone, Copy)]
pub(crate) struct ScopeData<'a> {
    shared: &'a Arc<Shared>,
    priority: Priority,
    first_error: &'a Mutex<Option<JobError>>,
}

impl ScopeData<'_> {
    fn record(&self, error: JobError) {
        let mut slot = self
            .first_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

/// Область задач, которые могут заимствовать данные вызывающего (см. [`ThreadPool::scope`]).
///
/// `Copy`: передаётся в замыкания по значению, поэтому задачи могут порождать новые задачи
/// той же области.
pub struct Scope<'scope, 'env: 'scope> {
    inner: &'scope TaskScope<'scope, 'env, ScopeData<'scope>>,
    env: PhantomData<&'env mut &'env ()>,
}

impl Clone for Scope<'_, '_> {
    fn clone(&self) -> Self {
        *self
    }
}

impl Copy for Scope<'_, '_> {}

impl std::fmt::Debug for Scope<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scope")
            .field("pending", &self.inner.pending())
            .finish_non_exhaustive()
    }
}

impl<'scope> Scope<'scope, '_> {
    /// Запускает задачу области. Паника задачи перехватывается; первая из них
    /// возвращается из [`ThreadPool::scope`] как ошибка.
    pub fn spawn<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'scope,
    {
        self.spawn_named("scope task", f);
    }

    pub(crate) fn spawn_named<F>(&self, name: &'static str, f: F)
    where
        F: FnOnce() + Send + 'scope,
    {
        let data: ScopeData<'scope> = *self.inner.data();
        let task = self.inner.task(move || {
            let labeled = || crate::labeled("scope", name, f);
            if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(labeled)) {
                data.record(JobError::panicked(name, &*payload));
            }
        });
        data.shared.push(Job::Scoped(task), data.priority);
    }

    /// Задача из слота на стеке вызывающего — без аллокации. Паники не перехватывает:
    /// замыкание в слоте обязано делать это само.
    fn spawn_slot<F>(&self, slot: &'scope StackSlot<F>)
    where
        F: FnOnce() + Send + 'scope,
    {
        let data: ScopeData<'scope> = *self.inner.data();
        if let Some(task) = self.inner.task_in(slot) {
            data.shared.push(Job::Scoped(task), data.priority);
        }
    }
}

impl ThreadPool {
    /// Область задач с заимствованием данных (без `'static`).
    ///
    /// Возвращается, только когда завершены все задачи области; пока ждёт — исполняет
    /// работу пула. Паника в `f` или в любой задаче — `Err(JobError::Panicked)`
    /// (возвращается первая перехваченная), остальные задачи всё равно доработают.
    pub fn scope<'env, F, R>(&self, f: F) -> Result<R, JobError>
    where
        F: for<'scope> FnOnce(Scope<'scope, 'env>) -> R,
    {
        observe!(self, "scope");
        self.scope_named("scope", f)
    }

    pub(crate) fn scope_named<'env, F, R>(&self, name: &'static str, f: F) -> Result<R, JobError>
    where
        F: for<'scope> FnOnce(Scope<'scope, 'env>) -> R,
    {
        let first_error = Mutex::new(None);
        let shared = &self.shared;
        let data = ScopeData {
            shared,
            priority: inherited_priority(),
            first_error: &first_error,
        };
        let body = enter(shared, || {
            pragma_sync::scope(
                data,
                |inner| {
                    panic::catch_unwind(AssertUnwindSafe(|| {
                        f(Scope {
                            inner,
                            env: PhantomData,
                        })
                    }))
                },
                || help_once(shared),
            )
        });
        match body {
            Err(payload) => Err(JobError::panicked(name, &*payload)),
            Ok(value) => first_error
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner)
                .map_or(Ok(value), Err),
        }
    }
    /// Fork-join: `a` исполняется на текущем потоке, `b` — в пуле (или тоже здесь, если его
    /// никто не украл). Вложенные `join` не дедлочат ни при каком числе воркеров.
    pub fn join<A, B, RA, RB>(&self, a: A, b: B) -> Result<(RA, RB), JobError>
    where
        A: FnOnce() -> RA,
        B: FnOnce() -> RB + Send,
        RB: Send,
    {
        let mut rb = None;
        // `b` лежит на стеке этой функции: `join` не аллоцирует (горячий путь вложенного
        // параллелизма). Паника `b` перехватывается здесь же и сообщается ниже.
        let slot = StackSlot::new(|| rb = Some(panic::catch_unwind(AssertUnwindSafe(b))));
        let ra = self.scope_named("join a", |s| {
            s.spawn_slot(&slot);
            a()
        });
        drop(slot);
        let ra = ra?;
        match rb {
            Some(Ok(rb)) => Ok((ra, rb)),
            Some(Err(payload)) => Err(JobError::panicked("join b", &*payload)),
            None => Err(JobError::cancelled("join b")),
        }
    }

    /// Параллельно применяет `f` к каждому элементу.
    pub fn parallel_for<T, F>(&self, items: &mut [T], grain: Grain, f: F) -> Result<(), JobError>
    where
        T: Send,
        F: Fn(&mut T) + Sync,
    {
        self.parallel_for_chunks(items, grain, |_, chunk| chunk.iter_mut().for_each(&f))
    }

    /// Параллельно обрабатывает чанки `items`; `f` получает смещение первого элемента чанка
    /// и сам чанк. Разбиение — см. [`Grain`].
    pub fn parallel_for_chunks<T, F>(
        &self,
        items: &mut [T],
        grain: Grain,
        f: F,
    ) -> Result<(), JobError>
    where
        T: Send,
        F: Fn(usize, &mut [T]) + Sync,
    {
        observe!(self, "parallel_for");
        let chunks = items.chunks_mut(grain.chunk_size(items.len()));
        let f = &f;
        self.scope_named("parallel_for", move |s| {
            let mut offset = 0_usize;
            for chunk in chunks {
                let start = offset;
                offset = offset.saturating_add(chunk.len());
                s.spawn_named("parallel_for chunk", move || f(start, chunk));
            }
        })
    }

    /// Детерминированная редукция: `map` считает частичный результат каждого чанка
    /// параллельно, `combine` сворачивает их на вызывающем потоке **строго в порядке чанков**.
    ///
    /// Разбиение не зависит от числа воркеров, поэтому даже неассоциативная свёртка
    /// (сумма `f32`/`f64`) даёт одинаковый результат на любом пуле. `None` — пустой вход.
    pub fn parallel_reduce<T, R, M, C>(
        &self,
        items: &[T],
        grain: Grain,
        map: M,
        mut combine: C,
    ) -> Result<Option<R>, JobError>
    where
        T: Sync,
        R: Send,
        M: Fn(usize, &[T]) -> R + Sync,
        C: FnMut(R, R) -> R,
    {
        observe!(self, "parallel_reduce");
        let size = grain.chunk_size(items.len());
        let mut parts: Vec<Option<R>> = Vec::new();
        parts.resize_with(items.len().div_ceil(size), || None);
        let work = items.chunks(size).zip(parts.iter_mut());
        let map = &map;
        self.scope_named("parallel_reduce", move |s| {
            let mut offset = 0_usize;
            for (chunk, slot) in work {
                let start = offset;
                offset = offset.saturating_add(chunk.len());
                s.spawn_named("parallel_reduce chunk", move || {
                    *slot = Some(map(start, chunk));
                });
            }
        })?;
        let mut acc: Option<R> = None;
        for part in parts {
            let part = part.ok_or_else(|| JobError::cancelled("parallel_reduce chunk"))?;
            acc = Some(match acc {
                None => part,
                Some(prev) => combine(prev, part),
            });
        }
        Ok(acc)
    }
}
