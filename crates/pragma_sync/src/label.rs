//! Labels of the tasks running on a thread: "which task is running here right now" for the panic
//! hook and crash handlers (DECISIONS #28).
//!
//! A label is a frame on the stack of [`run_labeled`]; the thread keeps in TLS only a pointer to the
//! innermost frame, and each frame points to the outer one. Setting and clearing a label are two TLS
//! writes, reading is one; there are no allocations and no copies of the name.

use std::cell::Cell;
use std::fmt;
use std::ptr;

/// A label frame on the stack of [`run_labeled`].
struct Frame {
    kind: &'static str,
    /// The name, borrowed from the caller of [`run_labeled`] for the duration of the call.
    name: *const str,
    /// The outer frame of the same thread (`null`: no labels above).
    parent: *const Frame,
}

thread_local! {
    /// The innermost frame of the current thread. It has no destructor, so it is available at any
    /// point of the thread's lifetime, including from the panic hook.
    static CURRENT: Cell<*const Frame> = const { Cell::new(ptr::null()) };
}

/// Restores the outer frame, also while the stack is unwinding from a panic.
struct Restore(*const Frame);

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = CURRENT.try_with(|current| current.set(self.0));
    }
}

/// Runs `f` with the task label `kind`/`name` on the current thread.
///
/// While `f` runs (and while its panic unwinds), [`current_label`] on this thread
/// sees this label, and through [`TaskLabel::parent`] the labels of outer `run_labeled` calls.
/// `kind` is the category (`"graph"`, `"spawn"`, `"async"`…), `name` is the task name.
pub fn run_labeled<R>(kind: &'static str, name: &str, f: impl FnOnce() -> R) -> R {
    let parent = CURRENT.try_with(Cell::get).unwrap_or(ptr::null());
    let frame = Frame {
        kind,
        name: ptr::from_ref(name),
        parent,
    };
    if CURRENT
        .try_with(|current| current.set(ptr::from_ref(&frame)))
        .is_err()
    {
        return f();
    }
    let _restore = Restore(parent);
    f()
}

/// Passes to `f` the label of the task currently running on this thread (`None`: no labels).
///
/// Neither allocates nor blocks, so it is fit for a panic hook. The label is borrowed only for
/// the duration of `f`: it (or the name) cannot be moved out; copy what you need.
///
/// Labels are set by the scheduler itself (pool tasks, graph nodes, async tasks, and
/// `pragma_jobs::labeled` for the application's own code). The example sets them by hand through
/// the internal `run_labeled` only to stay inside this crate:
///
/// ```
/// use pragma_sync::__private::run_labeled;
/// use pragma_sync::current_label;
///
/// run_labeled("graph", "physics", || {
///     run_labeled("scope", "parallel_for chunk", || {
///         current_label(|label| {
///             let label = label.expect("inside run_labeled");
///             assert_eq!(label.name(), "parallel_for chunk");
///             assert_eq!(label.parent().map(|p| p.name()), Some("physics"));
///             assert_eq!(label.to_string(), "scope `parallel_for chunk` <- graph `physics`");
///         });
///     });
/// });
/// assert!(current_label(|label| label.is_none()));
/// ```
///
/// A panic hook that names the culprit (the hook runs on the panicking thread, before unwinding):
///
/// ```
/// use pragma_sync::current_label;
///
/// let default_hook = std::panic::take_hook();
/// std::panic::set_hook(Box::new(move |info| {
///     current_label(|label| match label {
///         Some(label) => eprintln!("panic in task {label}"),
///         None => eprintln!("panic outside pool tasks"),
///     });
///     default_hook(info);
/// }));
/// # let _ = std::panic::take_hook();
/// ```
pub fn current_label<R>(f: impl for<'a> FnOnce(Option<TaskLabel<'a>>) -> R) -> R {
    let frame = CURRENT.try_with(Cell::get).unwrap_or(ptr::null());
    // SAFETY: `CURRENT` is either null or points to a `Frame` on the stack of a `run_labeled` call
    // that has not returned yet: the frame is set before `f` and cleared by `Restore` after it (also
    // during unwinding). We are inside that call on the same thread, so the frame is alive for the
    // whole of `f`, and `TaskLabel<'a>` cannot outlive `f` (the lifetime `'a` does not escape the
    // closure) nor leave the thread (`!Send`, `!Sync` because of the raw `Frame` pointers).
    let label = unsafe { frame.as_ref() }.map(|frame| TaskLabel { frame });
    f(label)
}

/// A task label (see [`current_label`]).
///
/// Borrowed from the thread's label stack for the duration of the [`current_label`] closure, so it
/// is `!Send` and cannot be stored; `Display` prints the whole chain, innermost first.
///
/// ```
/// use pragma_sync::__private::run_labeled;
/// use pragma_sync::current_label;
///
/// run_labeled("graph", "ai", || {
///     run_labeled("spawn", "path", || {
///         let kinds: Vec<&str> = current_label(|label| {
///             label.map(|l| l.chain().map(|l| l.kind()).collect()).unwrap_or_default()
///         });
///         assert_eq!(kinds, ["spawn", "graph"]);
///     });
/// });
/// ```
#[derive(Clone, Copy)]
pub struct TaskLabel<'a> {
    frame: &'a Frame,
}

impl<'a> TaskLabel<'a> {
    /// The task category: `"graph"`, `"spawn"`, `"async"`….
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.frame.kind
    }

    /// The task name.
    #[must_use]
    pub fn name(&self) -> &'a str {
        // SAFETY: `name` is borrowed by the `run_labeled` call that owns the frame and has not
        // returned yet (see `current_label`), so the string is alive for the whole of `'a`.
        unsafe { &*self.frame.name }
    }

    /// The label of the outer `run_labeled` call on the same thread. With help-while-wait this is not
    /// necessarily the logical parent: a waiting task runs other tasks on its own stack.
    #[must_use]
    pub fn parent(&self) -> Option<TaskLabel<'a>> {
        // SAFETY: the outer frame belongs to a `run_labeled` call further down the same thread's
        // stack: it lives longer than the inner one, and hence for the whole of `'a`.
        unsafe { self.frame.parent.as_ref() }.map(|frame| TaskLabel { frame })
    }

    /// This label and all outer ones, from the innermost to the outermost.
    pub fn chain(self) -> impl Iterator<Item = TaskLabel<'a>> {
        std::iter::successors(Some(self), TaskLabel::parent)
    }
}

impl fmt::Debug for TaskLabel<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskLabel")
            .field("kind", &self.kind())
            .field("name", &self.name())
            .finish()
    }
}

/// The label chain from the innermost to the outermost: ``graph `physics` <- spawn `bake` ``.
impl fmt::Display for TaskLabel<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, label) in self.chain().enumerate() {
            if i > 0 {
                f.write_str(" <- ")?;
            }
            write!(f, "{} `{}`", label.kind(), label.name())?;
        }
        Ok(())
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::panic::{self, AssertUnwindSafe};

    fn names() -> Vec<String> {
        current_label(|label| {
            label
                .map(|l| l.chain().map(|l| l.name().to_owned()).collect())
                .unwrap_or_default()
        })
    }

    #[test]
    fn no_label_outside() {
        assert!(names().is_empty());
    }

    #[test]
    fn nested_labels_form_a_chain_and_unwind() {
        let outer = String::from("outer");
        let value = run_labeled("graph", &outer, || {
            run_labeled("spawn", "inner", || {
                assert_eq!(names(), ["inner", "outer"]);
                current_label(|l| assert_eq!(l.map(|l| l.kind()), Some("spawn")));
            });
            assert_eq!(names(), ["outer"]);
            7
        });
        assert_eq!(value, 7);
        assert!(names().is_empty());
    }

    #[test]
    fn panic_restores_outer_label() {
        run_labeled("graph", "outer", || {
            let caught = panic::catch_unwind(AssertUnwindSafe(|| {
                run_labeled("spawn", "inner", || panic!("boom"));
            }));
            assert!(caught.is_err());
            assert_eq!(names(), ["outer"]);
        });
        assert!(names().is_empty());
    }

    #[test]
    fn labels_are_per_thread() {
        run_labeled("graph", "main-thread", || {
            let other = std::thread::spawn(|| {
                assert!(names().is_empty());
                run_labeled("spawn", "worker", names)
            })
            .join()
            .unwrap();
            assert_eq!(other, ["worker"]);
            assert_eq!(names(), ["main-thread"]);
        });
    }

    #[test]
    fn display_shows_chain() {
        run_labeled("graph", "a", || {
            run_labeled("async", "b", || {
                let text = current_label(|l| l.map(|l| l.to_string()));
                assert_eq!(text.as_deref(), Some("async `b` <- graph `a`"));
            });
        });
    }
}
