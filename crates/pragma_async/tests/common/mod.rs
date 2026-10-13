#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Считает сбросы помеченных значений: утечка future видна как недосчёт.
#[derive(Clone, Default)]
pub struct DropCounter(Arc<AtomicUsize>);

impl DropCounter {
    pub fn token(&self) -> DropToken {
        DropToken(Arc::clone(&self.0))
    }

    pub fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct DropToken(Arc<AtomicUsize>);

impl Drop for DropToken {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Ждёт условия; по таймауту падает (вместо зависшего теста).
pub fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}
