//! Метки задач, исполняемых на потоке: «какая задача сейчас здесь работает» для panic hook
//! и обработчиков падений (DECISIONS #28).
//!
//! Метка — кадр на стеке [`run_labeled`]; поток хранит в TLS только указатель на самый
//! внутренний кадр, кадр — указатель на внешний. Поставить и снять метку — две записи в TLS,
//! прочитать — одна; аллокаций и копирования имени нет.

use std::cell::Cell;
use std::fmt;
use std::ptr;

/// Кадр метки на стеке [`run_labeled`].
struct Frame {
    kind: &'static str,
    /// Имя, заимствованное у вызывающего [`run_labeled`] на время вызова.
    name: *const str,
    /// Внешний кадр этого же потока (`null` — меток выше нет).
    parent: *const Frame,
}

thread_local! {
    /// Самый внутренний кадр текущего потока. Без деструктора, поэтому доступен в любой
    /// момент жизни потока, в том числе из panic hook.
    static CURRENT: Cell<*const Frame> = const { Cell::new(ptr::null()) };
}

/// Возвращает внешний кадр на место и при раскрутке стека паникой.
struct Restore(*const Frame);

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = CURRENT.try_with(|current| current.set(self.0));
    }
}

/// Исполняет `f` с меткой задачи `kind`/`name` на текущем потоке.
///
/// Пока `f` исполняется (и пока раскручивается её паника), [`current_label`] на этом потоке
/// видит эту метку, а через [`TaskLabel::parent`] — метки внешних вызовов `run_labeled`.
/// `kind` — категория (`"graph"`, `"spawn"`, `"async"`…), `name` — имя задачи.
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

/// Передаёт `f` метку задачи, исполняемой сейчас на этом потоке (`None` — меток нет).
///
/// Не аллоцирует и не блокирует — годится для panic hook. Метка заимствована только на
/// время `f`: вынести её (или имя) наружу нельзя, скопируйте нужное.
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
pub fn current_label<R>(f: impl for<'a> FnOnce(Option<TaskLabel<'a>>) -> R) -> R {
    let frame = CURRENT.try_with(Cell::get).unwrap_or(ptr::null());
    // SAFETY: `CURRENT` указывает либо в никуда, либо на `Frame` в стеке вызова `run_labeled`,
    // который ещё не вернулся: кадр ставится перед `f` и снимается `Restore` после неё (и при
    // раскрутке). Мы — внутри этого вызова на том же потоке, поэтому кадр жив всё время `f`,
    // а `TaskLabel<'a>` не может пережить `f` (лайфтайм `'a` не выходит из замыкания) и не
    // покидает поток (`!Send`, `!Sync` из-за сырых указателей `Frame`).
    let label = unsafe { frame.as_ref() }.map(|frame| TaskLabel { frame });
    f(label)
}

/// Метка задачи (см. [`current_label`]).
#[derive(Clone, Copy)]
pub struct TaskLabel<'a> {
    frame: &'a Frame,
}

impl<'a> TaskLabel<'a> {
    /// Категория задачи: `"graph"`, `"spawn"`, `"async"`….
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.frame.kind
    }

    /// Имя задачи.
    #[must_use]
    pub fn name(&self) -> &'a str {
        // SAFETY: `name` заимствован вызовом `run_labeled`, который владеет кадром и ещё не
        // вернулся (см. `current_label`), поэтому строка жива на всё время `'a`.
        unsafe { &*self.frame.name }
    }

    /// Метка внешнего вызова `run_labeled` на этом же потоке. С help-while-wait это не
    /// обязательно логический родитель: ожидающая задача исполняет чужие задачи у себя на стеке.
    #[must_use]
    pub fn parent(&self) -> Option<TaskLabel<'a>> {
        // SAFETY: внешний кадр принадлежит вызову `run_labeled` ниже по стеку того же потока:
        // он живёт дольше внутреннего, а значит, и всё время `'a`.
        unsafe { self.frame.parent.as_ref() }.map(|frame| TaskLabel { frame })
    }

    /// Эта метка и все внешние, от внутренней к внешней.
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

/// Цепочка меток от внутренней к внешней: ``graph `physics` <- spawn `bake` ``.
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
