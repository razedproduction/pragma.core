use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

/// Счётная защёлка: `wait` блокирует, пока счётчик не дойдёт до нуля.
///
/// Базовый примитив для `join`/`scope`: ожидающий поток ждёт завершения N задач.
/// Быстрый путь (`is_released`) — одна атомарная загрузка без блокировок.
#[derive(Debug)]
pub struct CountLatch {
    remaining: AtomicUsize,
    lock: Mutex<()>,
    cvar: Condvar,
}

impl CountLatch {
    /// Защёлка, ожидающая `count` вызовов [`count_down`](Self::count_down).
    #[must_use]
    pub const fn new(count: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(count),
            lock: Mutex::new(()),
            cvar: Condvar::new(),
        }
    }

    /// Сколько вызовов `count_down` ещё ожидается.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.remaining.load(Ordering::Acquire)
    }

    /// Защёлка открыта (счётчик равен нулю).
    #[must_use]
    pub fn is_released(&self) -> bool {
        self.remaining() == 0
    }

    /// Уменьшает счётчик; при достижении нуля будит всех ожидающих.
    /// Вызов на уже открытой защёлке ничего не делает (без переполнения вниз).
    pub fn count_down(&self) {
        let mut current = self.remaining.load(Ordering::Acquire);
        // CAS-цикл вместо `fetch_update`/`try_update`: одинаково работает на MSRV и новых тулчейнах.
        let reached_zero = loop {
            let Some(next) = current.checked_sub(1) else {
                return;
            };
            match self.remaining.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break next == 0,
                Err(actual) => current = actual,
            }
        };
        if reached_zero {
            // Захват мьютекса исключает гонку «проверил счётчик → уснул → пропустил notify».
            let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
            self.cvar.notify_all();
        }
    }

    /// Блокирует текущий поток до открытия защёлки.
    pub fn wait(&self) {
        if self.is_released() {
            return;
        }
        let mut guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        while !self.is_released() {
            guard = self
                .cvar
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn zero_latch_is_released() {
        let latch = CountLatch::new(0);
        latch.wait();
        latch.count_down();
        assert_eq!(latch.remaining(), 0);
    }

    #[test]
    fn many_threads_release_once() {
        const N: usize = 64;
        let latch = Arc::new(CountLatch::new(N));
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let latch = Arc::clone(&latch);
                std::thread::spawn(move || latch.count_down())
            })
            .collect();
        latch.wait();
        for h in handles {
            h.join().unwrap();
        }
        assert!(latch.is_released());
    }
}
