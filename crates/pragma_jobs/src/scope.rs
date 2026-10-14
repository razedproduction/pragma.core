use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, PoisonError};

use pragma_sync::__private::{StackSlot, TaskScope};

use crate::pool::{Job, Shared, enter, help_once, inherited_priority, join_as_participant};
use crate::{Grain, JobError, Priority, ThreadPool};

/// The outcome of the two `join` branches: a panic of `a` takes precedence; `b` without a result was cancelled.
fn join_result<RA, RB>(
    (ra, rb): (std::thread::Result<RA>, Option<std::thread::Result<RB>>),
) -> Result<(RA, RB), JobError> {
    let ra = ra.map_err(|payload| JobError::panicked("join a", &*payload))?;
    match rb {
        Some(Ok(rb)) => Ok((ra, rb)),
        Some(Err(payload)) => Err(JobError::panicked("join b", &*payload)),
        None => Err(JobError::cancelled("join b")),
    }
}

/// A `tracing` span (level `TRACE`) and a puffin scope for the duration of pool operation `$op`.
/// The span fields are evaluated only if the level is enabled.
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

/// Scope data: everything is borrowed from `scope_named`, so the scope neither allocates nor
/// touches the `Arc<Shared>` reference count shared by all threads.
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

/// A scope of tasks that may borrow the caller's data (see [`ThreadPool::scope`]).
///
/// `Copy`: it is passed into closures by value, so tasks can spawn new tasks
/// in the same scope.
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
    /// Spawns a scope task. A task panic is caught; the first one
    /// is returned from [`ThreadPool::scope`] as an error.
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

    /// A task from a slot on the caller's stack, with no allocation. It does not catch panics:
    /// the closure in the slot must do that itself.
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
    /// A scope of tasks that borrow data (no `'static` needed).
    ///
    /// Returns only when all tasks of the scope have finished; while waiting it runs
    /// pool work. A panic in `f` or in any task gives `Err(JobError::Panicked)`
    /// (the first one caught is returned); the other tasks still run to completion.
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
            pragma_sync::__private::scope(
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
    /// Fork-join: `a` runs on the current thread, `b` in the pool (or here too, if nobody
    /// stole it). Nested `join`s do not deadlock with any number of workers.
    pub fn join<A, B, RA, RB>(&self, a: A, b: B) -> Result<(RA, RB), JobError>
    where
        A: FnOnce() -> RA,
        B: FnOnce() -> RB + Send,
        RB: Send,
    {
        // The hot path: already a pool participant (a nested `join`, a graph task).
        let (a, b) = match join_as_participant(&self.shared, a, b) {
            Ok(branches) => return join_result(branches),
            Err(branches) => branches,
        };
        // An external thread: take a slot and retry; without a free slot, go through a scope.
        enter(&self.shared, move || {
            match join_as_participant(&self.shared, a, b) {
                Ok(branches) => join_result(branches),
                Err((a, b)) => self.join_scoped(a, b),
            }
        })
    }

    /// `join` for a thread outside the pool with no free external slot: `b` is a scope task
    /// (through the global queue), and the wait is on the scope counter.
    fn join_scoped<A, B, RA, RB>(&self, a: A, b: B) -> Result<(RA, RB), JobError>
    where
        A: FnOnce() -> RA,
        B: FnOnce() -> RB + Send,
        RB: Send,
    {
        let mut rb = None;
        // `b` lives on this function's stack: `join` does not allocate (the hot path of nested
        // parallelism). A panic of `b` is caught right here and reported below.
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

    /// Applies `f` to every element in parallel.
    pub fn parallel_for<T, F>(&self, items: &mut [T], grain: Grain, f: F) -> Result<(), JobError>
    where
        T: Send,
        F: Fn(&mut T) + Sync,
    {
        self.parallel_for_chunks(items, grain, |_, chunk| chunk.iter_mut().for_each(&f))
    }

    /// Processes chunks of `items` in parallel; `f` receives the offset of the chunk's first element
    /// and the chunk itself. For the split, see [`Grain`].
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

    /// Deterministic reduction: `map` computes the partial result of each chunk
    /// in parallel, and `combine` folds them on the calling thread **strictly in chunk order**.
    ///
    /// The split does not depend on the number of workers, so even a non-associative fold
    /// (an `f32`/`f64` sum) gives the same result on any pool. `None` means an empty input.
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
