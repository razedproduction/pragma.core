use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::PoisonError;
use std::time::Duration;

use crate::primitives::{Arc, AtomicUsize, Condvar, Mutex, Ordering};

const EMPTY: usize = 0;
const PARKED: usize = 1;
const NOTIFIED: usize = 2;

#[derive(Debug)]
struct Inner {
    state: AtomicUsize,
    lock: Mutex<()>,
    cvar: Condvar,
}

/// Парковка потока: владелец засыпает в [`park`](Self::park), пока его не разбудит
/// [`Unparker::unpark`].
///
/// Пробуждение не теряется: `unpark` до `park` оставляет «жетон», и следующий `park`
/// возвращается сразу. Жетон один — несколько `unpark` подряд не накапливаются.
/// `Parker` не `Sync`: парковаться может только поток-владелец, будить — кто угодно.
#[derive(Debug)]
pub struct Parker {
    inner: Arc<Inner>,
    _not_sync: PhantomData<Cell<()>>,
}

/// Будильник для [`Parker`]; дешёво клонируется и передаётся между потоками.
#[derive(Debug, Clone)]
pub struct Unparker {
    inner: Arc<Inner>,
}

impl Default for Parker {
    fn default() -> Self {
        Self::new()
    }
}

impl Parker {
    /// Новый парковщик без жетона.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: AtomicUsize::new(EMPTY),
                lock: Mutex::new(()),
                cvar: Condvar::new(),
            }),
            _not_sync: PhantomData,
        }
    }

    /// Будильник для этого парковщика.
    #[must_use]
    pub fn unparker(&self) -> Unparker {
        Unparker {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Засыпает до `unpark`; если жетон уже есть — забирает его и возвращается сразу.
    pub fn park(&self) {
        self.park_inner(None);
    }

    /// Как [`park`](Self::park), но не дольше `timeout` (страховка от ошибок в протоколе
    /// пробуждения вызывающего кода). Возможны ложные пробуждения.
    pub fn park_timeout(&self, timeout: Duration) {
        self.park_inner(Some(timeout));
    }

    fn park_inner(&self, timeout: Option<Duration>) {
        let inner = &*self.inner;
        // Быстрый путь: жетон уже лежит.
        if inner
            .state
            .compare_exchange(NOTIFIED, EMPTY, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return;
        }
        let mut guard = inner.lock.lock().unwrap_or_else(PoisonError::into_inner);
        match inner
            .state
            .compare_exchange(EMPTY, PARKED, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => {}
            Err(_) => {
                // Жетон пришёл между быстрым путём и захватом мьютекса (единственный другой
                // возможный state, потому что парковаться может только владелец).
                inner.state.store(EMPTY, Ordering::SeqCst);
                return;
            }
        }
        match timeout {
            None => loop {
                guard = inner
                    .cvar
                    .wait(guard)
                    .unwrap_or_else(PoisonError::into_inner);
                if inner
                    .state
                    .compare_exchange(NOTIFIED, EMPTY, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return;
                }
                // Ложное пробуждение — спим дальше.
            },
            Some(timeout) => {
                let (_guard, _timed_out) = inner
                    .cvar
                    .wait_timeout(guard, timeout)
                    .unwrap_or_else(PoisonError::into_inner);
                // И таймаут, и уведомление приводят состояние в EMPTY: жетон потребляется.
                inner.state.store(EMPTY, Ordering::SeqCst);
            }
        }
    }
}

impl Unparker {
    /// Будит владельца парковщика (или оставляет жетон, если он ещё не спит).
    pub fn unpark(&self) {
        let inner = &*self.inner;
        match inner.state.swap(NOTIFIED, Ordering::SeqCst) {
            PARKED => {}
            // EMPTY или уже NOTIFIED: владелец не спит, жетона достаточно.
            _ => return,
        }
        // Захват мьютекса гарантирует, что владелец уже внутри `wait`, а не между
        // переходом в PARKED и засыпанием, — иначе notify ушёл бы в пустоту.
        drop(inner.lock.lock().unwrap_or_else(PoisonError::into_inner));
        inner.cvar.notify_one();
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn unpark_before_park_is_not_lost() {
        let p = Parker::new();
        p.unparker().unpark();
        p.park(); // вернулся сразу благодаря жетону
    }

    #[test]
    fn tokens_do_not_accumulate() {
        let p = Parker::new();
        let u = p.unparker();
        u.unpark();
        u.unpark();
        p.park();
        let start = Instant::now();
        p.park_timeout(Duration::from_millis(20));
        assert!(start.elapsed() >= Duration::from_millis(10));
    }

    #[test]
    fn cross_thread_wakeup() {
        let p = Parker::new();
        let u = p.unparker();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            u.unpark();
        });
        p.park();
        t.join().unwrap();
    }
}
