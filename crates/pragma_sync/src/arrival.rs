use crate::primitives::{AtomicUsize, Ordering};

/// Счётчик «ждём N прибытий»: последний пришедший узнаёт, что можно продолжать.
///
/// Основа dataflow-исполнения графа кадра: у задачи столько прибытий, сколько у неё
/// предшественников; предшественник, прибывший последним, запускает её. Прибытие — RMW с
/// `AcqRel`, поэтому последний прибывший видит всё, что сделали остальные до своего прибытия
/// (loom-модель в `tests/loom.rs`).
///
/// В отличие от [`AtomicCounter`](crate::AtomicCounter), ждать на нём нельзя — и не нужно:
/// никто не блокируется, нет мьютекса и `Condvar`, только один атомик.
#[derive(Debug)]
pub struct ArrivalCounter {
    remaining: AtomicUsize,
}

impl Default for ArrivalCounter {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ArrivalCounter {
    /// Счётчик на `expected` прибытий.
    #[must_use]
    pub fn new(expected: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(expected),
        }
    }

    /// Готовит счётчик к новому раунду. `&mut` гарантирует, что прибытий прошлого раунда
    /// в этот момент нет; новый раунд другим потокам публикует тот, кто отдаёт им работу.
    pub fn reset(&mut self, expected: usize) {
        self.remaining.store(expected, Ordering::Relaxed);
    }

    /// Регистрирует прибытие; `true` — оно последнее. Лишнее прибытие (на нуле) — ошибка
    /// вызывающего: ничего не меняет и возвращает `false`, а не заворачивается.
    pub fn arrive(&self) -> bool {
        let mut current = self.remaining.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_sub(1) else {
                debug_assert!(false, "ArrivalCounter: more arrivals than expected");
                return false;
            };
            match self.remaining.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next == 0,
                Err(actual) => current = actual,
            }
        }
    }

    /// Сколько прибытий ещё ждём.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.remaining.load(Ordering::Acquire)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize as StdAtomic;

    #[test]
    fn last_arrival_is_reported_once() {
        let mut c = ArrivalCounter::new(3);
        assert!(!c.arrive());
        assert!(!c.arrive());
        assert!(c.arrive());
        assert_eq!(c.remaining(), 0);
        c.reset(1);
        assert!(c.arrive());
    }

    #[test]
    fn exactly_one_thread_sees_the_last_arrival() {
        const N: usize = 16;
        for _ in 0..100 {
            let c = Arc::new(ArrivalCounter::new(N));
            let winners = Arc::new(StdAtomic::new(0));
            let threads: Vec<_> = (0..N)
                .map(|_| {
                    let (c, winners) = (Arc::clone(&c), Arc::clone(&winners));
                    std::thread::spawn(move || {
                        if c.arrive() {
                            winners.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(winners.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }
}
