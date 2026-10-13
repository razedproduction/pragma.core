use std::sync::PoisonError;
use std::time::Duration;

use crossbeam_utils::Backoff;

use crate::SyncError;
use crate::primitives::{AtomicUsize, Condvar, Mutex, Ordering};

/// Как часто ожидающий в [`AtomicCounter::wait_while_helping`] перепроверяет, не появилась ли
/// работа, пока он спит на счётчике.
const HELP_RECHECK: Duration = Duration::from_micros(500);

/// Счётчик незавершённых задач, на котором можно ждать нуля.
///
/// Вместо хэндла на каждую задачу: группа задач увеличивает счётчик при создании и уменьшает
/// при завершении, ожидающий ждёт одного события «счётчик стал нулём».
///
/// Протокол пробуждения: ожидающий регистрируется в `waiters` (RMW) под мьютексом и
/// перепроверяет счётчик; уменьшающий после перехода в ноль читает `waiters` тоже через RMW и
/// только при ненулевом значении берёт мьютекс. RMW по `waiters` упорядочены, поэтому хотя бы
/// одна сторона видит действие другой (loom-модель в `tests/loom.rs`). Без ожидающих переход
/// в ноль не трогает мьютекс.
#[derive(Debug)]
pub struct AtomicCounter {
    count: AtomicUsize,
    waiters: AtomicUsize,
    lock: Mutex<()>,
    cvar: Condvar,
}

impl Default for AtomicCounter {
    fn default() -> Self {
        Self::new(0)
    }
}

impl AtomicCounter {
    /// Счётчик с начальным значением `count`.
    #[must_use]
    pub fn new(count: usize) -> Self {
        Self {
            count: AtomicUsize::new(count),
            waiters: AtomicUsize::new(0),
            lock: Mutex::new(()),
            cvar: Condvar::new(),
        }
    }

    /// Текущее значение.
    #[must_use]
    pub fn get(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Счётчик равен нулю (все задачи завершены).
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.get() == 0
    }

    /// Увеличивает счётчик на единицу; ошибка вместо заворачивания на `usize::MAX`.
    pub fn increment(&self) -> Result<(), SyncError> {
        let mut current = self.count.load(Ordering::Relaxed);
        // CAS-цикл вместо `fetch_update`: тот устарел в 1.99, а `try_update` нет в MSRV.
        loop {
            let next = current.checked_add(1).ok_or(SyncError::CounterOverflow)?;
            match self.count.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    /// Уменьшает счётчик; возвращает `true`, если он стал нулём (тогда будит ожидающих).
    /// На нулевом счётчике ничего не делает и возвращает `false`.
    pub fn decrement(&self) -> bool {
        let mut current = self.count.load(Ordering::Acquire);
        let reached_zero = loop {
            let Some(next) = current.checked_sub(1) else {
                return false;
            };
            match self.count.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => break next == 0,
                Err(actual) => current = actual,
            }
        };
        // RMW, а не load: чтение через `fetch_add(0)` всегда видит последнее значение в порядке
        // модификаций `waiters`, и протокол остаётся верным даже без глобального порядка SeqCst
        // (loom моделирует SeqCst как AcqRel и ловит потерянное пробуждение у варианта с load).
        if reached_zero && self.waiters.fetch_add(0, Ordering::SeqCst) > 0 {
            // Ожидающий держит мьютекс от проверки счётчика до засыпания в `wait`,
            // поэтому notify под мьютексом не может проскочить мимо него.
            let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
            self.cvar.notify_all();
        }
        reached_zero
    }

    /// Блокирует поток, пока счётчик не станет нулём.
    pub fn wait(&self) {
        self.wait_inner(None);
    }

    /// Ждёт нуля не дольше `timeout`; возвращает `true`, если счётчик уже ноль.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        self.wait_inner(Some(timeout))
    }

    /// Ждёт нуля, а пока ждёт — вызывает `help` (help-while-wait).
    ///
    /// `help` должен выполнить одну единицу чужой работы и вернуть `true`, или вернуть `false`,
    /// если работы нет. Без работы поток сначала крутится с [`Backoff`], потом спит на счётчике
    /// короткими интервалами, между которыми снова спрашивает `help`.
    pub fn wait_while_helping(&self, mut help: impl FnMut() -> bool) {
        let backoff = Backoff::new();
        while !self.is_zero() {
            if help() {
                backoff.reset();
            } else if backoff.is_completed() {
                self.wait_timeout(HELP_RECHECK);
            } else {
                backoff.snooze();
            }
        }
    }

    fn wait_inner(&self, timeout: Option<Duration>) -> bool {
        if self.is_zero() {
            return true;
        }
        let mut guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.waiters.fetch_add(1, Ordering::SeqCst);
        match timeout {
            None => {
                while self.count.load(Ordering::SeqCst) != 0 {
                    guard = self
                        .cvar
                        .wait(guard)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
            Some(timeout) => {
                if self.count.load(Ordering::SeqCst) != 0 {
                    guard = self
                        .cvar
                        .wait_timeout(guard, timeout)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        }
        self.waiters.fetch_sub(1, Ordering::SeqCst);
        drop(guard);
        self.is_zero()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn increment_decrement_roundtrip() {
        let c = AtomicCounter::new(0);
        c.increment().unwrap();
        c.increment().unwrap();
        assert_eq!(c.get(), 2);
        assert!(!c.decrement());
        assert!(c.decrement());
        assert!(!c.decrement(), "decrement on zero is a no-op");
        assert!(c.is_zero());
    }

    #[test]
    fn overflow_is_an_error() {
        let c = AtomicCounter::new(usize::MAX);
        assert_eq!(c.increment(), Err(SyncError::CounterOverflow));
    }

    #[test]
    fn wait_wakes_on_zero() {
        const N: usize = 32;
        let c = Arc::new(AtomicCounter::new(N));
        let threads: Vec<_> = (0..N)
            .map(|_| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    c.decrement();
                })
            })
            .collect();
        c.wait();
        assert!(c.is_zero());
        for t in threads {
            t.join().unwrap();
        }
    }

    #[test]
    fn helper_runs_until_zero() {
        let c = AtomicCounter::new(3);
        let mut calls = 0;
        c.wait_while_helping(|| {
            calls += 1;
            c.decrement();
            true
        });
        assert_eq!(calls, 3);
    }

    #[test]
    fn wait_timeout_reports_state() {
        let c = AtomicCounter::new(1);
        assert!(!c.wait_timeout(Duration::from_millis(1)));
        c.decrement();
        assert!(c.wait_timeout(Duration::from_millis(1)));
    }
}
