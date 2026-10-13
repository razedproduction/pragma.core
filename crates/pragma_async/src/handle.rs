use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use async_task::FallibleTask;
use pragma_jobs::Spawner;

use crate::AsyncError;

pub(crate) type TaskOutput<T> = Result<T, AsyncError>;

enum State<T> {
    /// Задача в работе (или результат ещё не забран из `async-task`).
    Running(FallibleTask<TaskOutput<T>>),
    /// Исход известен сразу (задача не была поставлена).
    Ready(TaskOutput<T>),
    /// Результат отдан.
    Taken,
}

/// Хэндл async-задачи: [`Executor::spawn_async`](crate::Executor::spawn_async) и
/// [`Executor::unblock`](crate::Executor::unblock).
///
/// - **Сброс хэндла отменяет задачу**: future сбрасывается на исполнителе, не дожидаясь
///   завершения (если он сейчас в `poll`, то сразу после него). Чтобы задача доработала без
///   хэндла — [`detach`](Self::detach).
/// - Мост в кадр — [`poll_ready`](Self::poll_ready): неблокирующая проверка в начале кадра.
/// - Из другой async-задачи хэндл ожидается через `.await` (он сам реализует `Future`).
/// - [`wait`](Self::wait) блокирует поток, но только вне работы пула.
#[must_use = "сброс хэндла отменяет задачу; используйте detach(), чтобы она доработала"]
pub struct AsyncHandle<T> {
    state: State<T>,
    spawner: Spawner,
}

// Хэндл не проецирует закрепление внутрь `T`: результат лишь перемещается наружу.
impl<T> Unpin for AsyncHandle<T> {}

impl<T> std::fmt::Debug for AsyncHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncHandle")
            .field("finished", &self.is_finished())
            .finish()
    }
}

impl<T> AsyncHandle<T> {
    pub(crate) fn running(task: FallibleTask<TaskOutput<T>>, spawner: Spawner) -> Self {
        Self {
            state: State::Running(task),
            spawner,
        }
    }

    pub(crate) fn ready(result: TaskOutput<T>, spawner: Spawner) -> Self {
        Self {
            state: State::Ready(result),
            spawner,
        }
    }

    /// Задача завершилась (успешно, паникой или отменой) — [`poll_ready`](Self::poll_ready)
    /// вернёт `Some`.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        match &self.state {
            State::Running(task) => task.is_finished(),
            State::Ready(_) | State::Taken => true,
        }
    }

    /// Неблокирующая проверка: `Some(результат)`, если задача завершилась, иначе `None`.
    ///
    /// Это мост «async → кадр»: вызывается, например, в начале кадра. Результат отдаётся
    /// один раз; повторный вызов вернёт `Some(Err(AsyncError::Consumed))`.
    pub fn poll_ready(&mut self) -> Option<Result<T, AsyncError>> {
        let mut cx = Context::from_waker(Waker::noop());
        match self.poll_inner(&mut cx) {
            Poll::Ready(result) => Some(result),
            Poll::Pending => None,
        }
    }

    /// Блокирует текущий поток до завершения задачи.
    ///
    /// Только **вне** работы пула: из воркера или из кадра графа (главный поток внутри
    /// `scope`/`join`) вернёт [`AsyncError::WouldBlock`], не дожидаясь, — блокировка там
    /// задержала бы кадр, а на одном воркере повесила бы пул. Например, экран загрузки
    /// между кадрами.
    pub fn wait(self) -> Result<T, AsyncError> {
        if self.spawner.in_pool_context() {
            return Err(AsyncError::WouldBlock);
        }
        futures_lite::future::block_on(self)
    }

    /// Отпускает задачу: она доработает без хэндла, результат будет сброшен.
    pub fn detach(self) {
        if let State::Running(task) = self.state {
            task.detach();
        }
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<TaskOutput<T>> {
        let result = match &mut self.state {
            State::Running(task) => match Pin::new(task).poll(cx) {
                Poll::Pending => return Poll::Pending,
                // `None`: `Runnable` сброшен без завершения — отмена.
                Poll::Ready(output) => output.unwrap_or(Err(AsyncError::Cancelled)),
            },
            State::Ready(_) => match std::mem::replace(&mut self.state, State::Taken) {
                State::Ready(result) => result,
                State::Running(_) | State::Taken => Err(AsyncError::Consumed),
            },
            State::Taken => return Poll::Ready(Err(AsyncError::Consumed)),
        };
        self.state = State::Taken;
        Poll::Ready(result)
    }
}

impl<T> Future for AsyncHandle<T> {
    type Output = Result<T, AsyncError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_inner(cx)
    }
}
