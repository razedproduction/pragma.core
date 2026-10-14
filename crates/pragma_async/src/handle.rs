use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use async_task::FallibleTask;
use pragma_jobs::Spawner;

use crate::AsyncError;

pub(crate) type TaskOutput<T> = Result<T, AsyncError>;

enum State<T> {
    /// The task is running (or its result has not been taken from `async-task` yet).
    Running(FallibleTask<TaskOutput<T>>),
    /// The outcome is known right away (the task was never queued).
    Ready(TaskOutput<T>),
    /// The result has been handed out.
    Taken,
}

/// A handle to an async task: [`Executor::spawn_async`](crate::Executor::spawn_async) and
/// [`Executor::unblock`](crate::Executor::unblock).
///
/// - **Dropping the handle cancels the task**: the future is dropped on the executor without waiting
///   for completion (if it is inside `poll` right now, then right after it). To let the task run to
///   completion without a handle, use [`detach`](Self::detach).
/// - The bridge into the frame is [`poll_ready`](Self::poll_ready): a non-blocking check at the start of a frame.
/// - From another async task the handle is awaited with `.await` (it implements `Future` itself).
/// - [`wait`](Self::wait) blocks the thread, but only outside pool work.
#[must_use = "dropping the handle cancels the task; use detach() to let it run to completion"]
pub struct AsyncHandle<T> {
    state: State<T>,
    spawner: Spawner,
}

// The handle does not project pinning into `T`: the result is only moved out.
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

    /// Whether the task has finished (successfully, by panic or by cancellation), i.e.
    /// [`poll_ready`](Self::poll_ready) will return `Some`.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        match &self.state {
            State::Running(task) => task.is_finished(),
            State::Ready(_) | State::Taken => true,
        }
    }

    /// A non-blocking check: `Some(result)` if the task has finished, otherwise `None`.
    ///
    /// This is the "async → frame" bridge: called, for example, at the start of a frame. The result is
    /// handed out once; a repeated call returns `Some(Err(AsyncError::Consumed))`.
    pub fn poll_ready(&mut self) -> Option<Result<T, AsyncError>> {
        let mut cx = Context::from_waker(Waker::noop());
        match self.poll_inner(&mut cx) {
            Poll::Ready(result) => Some(result),
            Poll::Pending => None,
        }
    }

    /// Blocks the current thread until the task finishes.
    ///
    /// Only **outside** pool work: from a worker or from a graph frame (the main thread inside
    /// `scope`/`join`) it returns [`AsyncError::WouldBlock`] without waiting, because blocking there
    /// would delay the frame, and on a single worker it would hang the pool. A loading screen
    /// between frames is an example of a valid use.
    pub fn wait(self) -> Result<T, AsyncError> {
        if self.spawner.in_pool_context() {
            return Err(AsyncError::WouldBlock);
        }
        futures_lite::future::block_on(self)
    }

    /// Lets go of the task: it runs to completion without a handle, and its result is discarded.
    pub fn detach(self) {
        if let State::Running(task) = self.state {
            task.detach();
        }
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<TaskOutput<T>> {
        let result = match &mut self.state {
            State::Running(task) => match Pin::new(task).poll(cx) {
                Poll::Pending => return Poll::Pending,
                // `None`: the `Runnable` was dropped without completing, i.e. cancellation.
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
