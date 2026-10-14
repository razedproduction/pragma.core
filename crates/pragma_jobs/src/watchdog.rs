//! Watchdog зависших задач — только debug-сборки (весь модуль под `cfg(debug_assertions)`).
//!
//! У каждого потока пула (воркеры и внешние слоты) есть ячейка «какая задача с меткой
//! исполняется и с какого момента». [`labeled`](crate::labeled) заполняет её на входе и
//! восстанавливает внешнюю на выходе; поток watchdog раз в `period` просматривает ячейки и
//! предупреждает об одной и той же задаче один раз. Ячейки не аллоцируют: имя копируется в
//! буфер фиксированного размера, поэтому кадр графа и в debug остаётся без аллокаций.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use pragma_base::tracing;

/// Сколько байт имени задачи попадает в предупреждение.
const NAME_CAP: usize = 64;

/// Задача, исполняемая потоком сейчас.
#[derive(Clone, Copy)]
pub(crate) struct Active {
    kind: &'static str,
    name: [u8; NAME_CAP],
    len: usize,
    since: Instant,
    warned: bool,
}

impl Active {
    fn new(kind: &'static str, name: &str) -> Self {
        let mut len = name.len().min(NAME_CAP);
        while !name.is_char_boundary(len) {
            len = len.saturating_sub(1);
        }
        let mut buf = [0_u8; NAME_CAP];
        if let (Some(dst), Some(src)) = (buf.get_mut(..len), name.as_bytes().get(..len)) {
            dst.copy_from_slice(src);
        }
        Self {
            kind,
            name: buf,
            len,
            since: Instant::now(),
            warned: false,
        }
    }

    fn name(&self) -> &str {
        self.name
            .get(..self.len)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("?")
    }
}

pub(crate) struct Watchdog {
    threshold: Duration,
    slots: Box<[Mutex<Option<Active>>]>,
    /// Имя потока для каждой ячейки.
    threads: Box<[String]>,
}

impl Watchdog {
    pub(crate) fn new(threshold: Duration, threads: Vec<String>) -> Self {
        Self {
            threshold,
            slots: threads.iter().map(|_| Mutex::new(None)).collect(),
            threads: threads.into_boxed_slice(),
        }
    }

    /// Как часто просматривать ячейки: задержка предупреждения — не больше четверти порога.
    pub(crate) fn period(&self) -> Duration {
        let quarter = self.threshold.checked_div(4).unwrap_or(self.threshold);
        quarter.clamp(Duration::from_millis(1), Duration::from_secs(1))
    }

    /// Отмечает начало задачи на потоке `slot`; возвращает прежнее содержимое ячейки.
    pub(crate) fn enter(&self, slot: usize, kind: &'static str, name: &str) -> Option<Active> {
        let cell = self.slots.get(slot)?;
        cell.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(Active::new(kind, name))
    }

    /// Восстанавливает ячейку после задачи (внешнюю задачу — с её исходным временем начала).
    pub(crate) fn exit(&self, slot: usize, previous: Option<Active>) {
        if let Some(cell) = self.slots.get(slot) {
            *cell.lock().unwrap_or_else(PoisonError::into_inner) = previous;
        }
    }

    /// Предупреждает о задачах, исполняющихся дольше порога (каждой — один раз).
    pub(crate) fn scan(&self) {
        for (cell, thread) in self.slots.iter().zip(self.threads.iter()) {
            let hung = {
                let mut slot = cell.lock().unwrap_or_else(PoisonError::into_inner);
                match slot.as_mut() {
                    Some(active) if !active.warned && active.since.elapsed() >= self.threshold => {
                        active.warned = true;
                        Some(*active)
                    }
                    _ => None,
                }
            };
            if let Some(active) = hung {
                tracing::warn!(
                    target: "pragma_jobs::watchdog",
                    task = active.name(),
                    kind = active.kind,
                    thread = thread.as_str(),
                    elapsed_ms = u64::try_from(active.since.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "task runs longer than the watchdog threshold; it may be hung"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_are_truncated_on_char_boundary() {
        let name = "ж".repeat(40); // 80 байт
        let active = Active::new("graph", &name);
        assert!(active.name().len() <= NAME_CAP);
        assert!(name.starts_with(active.name()));
    }

    #[test]
    fn nested_enter_restores_outer() {
        let w = Watchdog::new(Duration::from_secs(1), vec![String::from("t")]);
        let outer = w.enter(0, "graph", "outer");
        assert!(outer.is_none());
        let saved = w.enter(0, "spawn", "inner");
        assert_eq!(saved.map(|a| a.len), Some(5));
        w.exit(0, saved);
        w.exit(0, outer);
        assert!(w.slots.first().unwrap().lock().unwrap().is_none());
    }
}
