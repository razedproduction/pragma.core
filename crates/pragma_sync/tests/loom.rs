//! loom-модели атомарных протоколов `pragma_sync`.
//!
//! Запуск: `RUSTFLAGS="--cfg loom" cargo test -p pragma_sync --release --lib --tests`.
//! Каждая модель перебирает все допустимые чередования потоков; зависание (потерянное
//! пробуждение) loom сообщает как дедлок.
#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;
use pragma_sync::{AtomicCounter, CountLatch, Parker};

#[test]
fn latch_releases_waiter() {
    loom::model(|| {
        let latch = Arc::new(CountLatch::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let latch = Arc::clone(&latch);
                thread::spawn(move || latch.count_down())
            })
            .collect();
        latch.wait();
        assert!(latch.is_released());
        for t in threads {
            t.join().unwrap();
        }
    });
}

#[test]
fn parker_never_loses_wakeup() {
    loom::model(|| {
        let parker = Parker::new();
        let unparker = parker.unparker();
        let flag = Arc::new(AtomicUsize::new(0));
        let t = {
            let flag = Arc::clone(&flag);
            thread::spawn(move || {
                flag.store(1, Ordering::Release);
                unparker.unpark();
            })
        };
        // Классический цикл ожидания условия: unpark после store не может потеряться.
        while flag.load(Ordering::Acquire) == 0 {
            parker.park();
        }
        t.join().unwrap();
    });
}

#[test]
fn parker_timeout_path_never_loses_wakeup() {
    loom::model(|| {
        let parker = Parker::new();
        let unparker = parker.unparker();
        let flag = Arc::new(AtomicUsize::new(0));
        let t = {
            let flag = Arc::clone(&flag);
            thread::spawn(move || {
                flag.store(1, Ordering::Release);
                unparker.unpark();
            })
        };
        // В loom таймаут не срабатывает, поэтому модель проверяет, что ветка
        // `park_timeout` тоже не теряет пробуждение.
        while flag.load(Ordering::Acquire) == 0 {
            parker.park_timeout(std::time::Duration::from_millis(1));
        }
        t.join().unwrap();
    });
}

#[test]
fn counter_wait_sees_last_decrement() {
    loom::model(|| {
        let counter = Arc::new(AtomicCounter::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let counter = Arc::clone(&counter);
                thread::spawn(move || {
                    counter.decrement();
                })
            })
            .collect();
        counter.wait();
        assert!(counter.is_zero());
        for t in threads {
            t.join().unwrap();
        }
    });
}

#[test]
fn counter_increment_while_waiting() {
    loom::model(|| {
        let counter = Arc::new(AtomicCounter::new(1));
        let t = {
            let counter = Arc::clone(&counter);
            thread::spawn(move || {
                // Задача порождает дочернюю до своего завершения — счётчик не падает в ноль раньше времени.
                counter.increment().unwrap();
                counter.decrement();
                counter.decrement();
            })
        };
        counter.wait();
        assert_eq!(counter.get(), 0);
        t.join().unwrap();
    });
}
