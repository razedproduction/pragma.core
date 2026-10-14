//! loom-модели атомарных протоколов `pragma_sync`.
//!
//! Запуск: `RUSTFLAGS="--cfg loom" cargo test -p pragma_sync --release --lib --tests`.
//! Каждая модель перебирает все допустимые чередования потоков; зависание (потерянное
//! пробуждение) loom сообщает как дедлок.
#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::Mutex;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;
use pragma_sync::{ArrivalCounter, AtomicCounter, CountLatch, Parker, StackJob, join_on_stack};

#[test]
fn stack_join_owner_never_misses_thief() {
    // Протокол латча `join_on_stack`: владелец помогает, затем объявляет сон и паркуется;
    // вор исполняет задачу и, если владелец объявил сон, будит его. Дедлок = потерянное
    // пробуждение; после возврата владельца кадр уничтожен — вор его уже не трогает.
    loom::model(|| {
        let parker = Parker::new();
        let unparker = parker.unparker();
        let queue: Arc<Mutex<Option<StackJob>>> = Arc::new(Mutex::new(None));
        let thief = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                loop {
                    if let Some(job) = queue.lock().unwrap().take() {
                        if job.run() == Some(0) {
                            unparker.unpark();
                        }
                        return;
                    }
                    thread::yield_now();
                }
            })
        };
        let data = 41_usize;
        let ((), b) = join_on_stack(
            0,
            || data + 1,
            |job| *queue.lock().unwrap() = Some(job),
            |latch| {
                if latch.prepare_park() {
                    parker.park();
                }
            },
        );
        assert_eq!(b.and_then(Result::ok), Some(42));
        thief.join().unwrap();
    });
}

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
fn last_arrival_sees_every_predecessor() {
    // Граф кадра: две задачи-предшественника пишут свои данные (Relaxed — без собственной
    // синхронизации) и прибывают; прибывший последним «запускает последователя» и обязан
    // увидеть обе записи. Ровно один из двух — последний.
    loom::model(|| {
        let arrival = Arc::new(ArrivalCounter::new(2));
        let data: Arc<[AtomicUsize; 2]> = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let threads: Vec<_> = (0..2)
            .map(|i| {
                let (arrival, data) = (Arc::clone(&arrival), Arc::clone(&data));
                thread::spawn(move || {
                    data[i].store(i + 1, Ordering::Relaxed);
                    if arrival.arrive() {
                        assert_eq!(data[0].load(Ordering::Relaxed), 1);
                        assert_eq!(data[1].load(Ordering::Relaxed), 2);
                        true
                    } else {
                        false
                    }
                })
            })
            .collect();
        let last: usize = threads
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum();
        assert_eq!(last, 1);
        assert_eq!(arrival.remaining(), 0);
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
