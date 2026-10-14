//! A watchdog for stuck tasks, debug builds only (the whole module is under `cfg(debug_assertions)`).
//!
//! Every pool thread (workers and external slots) has a cell saying "which labeled task is
//! running and since when". [`labeled`](crate::labeled) fills it on entry and
//! restores the outer one on exit; the watchdog thread scans the cells every `period` and
//! warns about any given task once. The cells do not allocate: the name is copied into a
//! fixed-size buffer, so a graph frame stays allocation-free in debug too.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How many bytes of the task name make it into the warning.
const NAME_CAP: usize = 64;

/// The task the thread is running right now.
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
    /// The thread name for each cell.
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

    /// How often to scan the cells: the warning is delayed by at most a quarter of the threshold.
    pub(crate) fn period(&self) -> Duration {
        let quarter = self.threshold.checked_div(4).unwrap_or(self.threshold);
        quarter.clamp(Duration::from_millis(1), Duration::from_secs(1))
    }

    /// Marks the start of a task on thread `slot`; returns the previous contents of the cell.
    pub(crate) fn enter(&self, slot: usize, kind: &'static str, name: &str) -> Option<Active> {
        let cell = self.slots.get(slot)?;
        cell.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(Active::new(kind, name))
    }

    /// Restores the cell after a task (the outer task keeps its original start time).
    pub(crate) fn exit(&self, slot: usize, previous: Option<Active>) {
        if let Some(cell) = self.slots.get(slot) {
            *cell.lock().unwrap_or_else(PoisonError::into_inner) = previous;
        }
    }

    /// Warns about tasks running longer than the threshold (once per task).
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
        let name = "\u{436}".repeat(40); // a 2-byte char: 80 bytes
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
