//! The stuck-task watchdog: in debug, one warning with the task name; in release, nothing.
//!
//! A separate binary with a single test: the `tracing` subscriber is global (the warning is written by
//! the watchdog thread). CI checks the release branch: `cargo test -p pragma_jobs --release --test watchdog`.
// Test code: a panic is a test failure.
#![allow(clippy::unwrap_used)]

use std::fmt::{self, Write as _};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use pragma_jobs::{PoolConfig, Priority, ThreadPool};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{self, Event, Metadata, Subscriber};

static WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A subscriber that collects watchdog warnings as `field=value` strings.
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

    // Fast tasks produce no warnings.
    for _ in 0..100 {
        pool.spawn(Priority::Normal, || ()).join().unwrap();
    }
    assert!(warnings().is_empty(), "{:?}", warnings());

    // A "stuck" task: 8 times longer than the threshold, but only one warning.
    // Wait without `join`: it helps run tasks and could take this task onto the main thread
    // itself (then the warning would come from `external-0`, not from a worker).
    let stuck = pool.spawn_named("stuck-asset-decode", Priority::Normal, || {
        std::thread::sleep(Duration::from_millis(400));
    });
    while !stuck.is_finished() {
        std::thread::sleep(Duration::from_millis(5));
    }
    stuck.join().unwrap();
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
