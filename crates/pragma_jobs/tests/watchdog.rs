//! Watchdog зависших задач: в debug — одно предупреждение с именем задачи, в release — ничего.
//!
//! Отдельный бинарник с одним тестом: подписчик `tracing` глобальный (предупреждение пишет
//! поток watchdog). Release-ветку проверяет CI: `cargo test -p pragma_jobs --release --test watchdog`.
// Тестовый код: паника = провал теста.
#![allow(clippy::unwrap_used)]

use std::fmt::{self, Write as _};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use pragma_base::tracing::field::{Field, Visit};
use pragma_base::tracing::span::{Attributes, Id, Record};
use pragma_base::tracing::{self, Event, Metadata, Subscriber};
use pragma_jobs::{PoolConfig, Priority, ThreadPool};

static WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Подписчик, который собирает предупреждения watchdog как строки `поле=значение`.
struct Capture;

struct Fields(String);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        if event.metadata().target() == "pragma_jobs::watchdog" {
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            WARNINGS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(fields.0);
        }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

fn warnings() -> Vec<String> {
    WARNINGS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

#[test]
fn hung_task_is_reported_once() {
    tracing::subscriber::set_global_default(Capture).unwrap();
    let pool = ThreadPool::new(
        PoolConfig::default()
            .with_workers(2)
            .with_watchdog(Some(Duration::from_millis(50))),
    )
    .unwrap();

    // Быстрые задачи предупреждений не дают.
    for _ in 0..100 {
        pool.spawn(Priority::Normal, || ()).join().unwrap();
    }
    assert!(warnings().is_empty(), "{:?}", warnings());

    // «Зависшая» задача: в 8 раз дольше порога, но предупреждение одно.
    pool.spawn_named("stuck-asset-decode", Priority::Normal, || {
        std::thread::sleep(Duration::from_millis(400));
    })
    .join()
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    drop(pool);

    let warnings = warnings();
    if cfg!(debug_assertions) {
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = warnings.first().unwrap();
        assert!(warning.contains("task=\"stuck-asset-decode\""), "{warning}");
        assert!(warning.contains("kind=\"spawn\""), "{warning}");
        assert!(warning.contains("thread=\"pragma-worker-"), "{warning}");
    } else {
        assert!(
            warnings.is_empty(),
            "release builds have no watchdog: {warnings:?}"
        );
    }
}
