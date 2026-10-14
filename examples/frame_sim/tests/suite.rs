//! The mandatory determinism suite in a debug build: here the watchdog actually works (the
//! 1 ms threshold fires), and the `tracing` subscriber enables a span per task; the hash does not change.
//! The same suite in release at full size is `frame_sim --suite` in CI.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use frame_sim::{Verdict, compare, determinism_suite, run};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

struct CountingSubscriber;

static SPANS: AtomicU64 = AtomicU64::new(0);

impl Subscriber for CountingSubscriber {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(SPANS.fetch_add(1, Ordering::Relaxed) + 1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// A single test function: the subscriber is global. All runs have the subscriber, except the reference.
#[test]
fn suite_hashes_match_in_debug() {
    let scenarios = determinism_suite(16, 4_000, &[1, 2, 3], Duration::from_millis(40));
    assert_eq!(scenarios.len(), 5);
    let references: Vec<Vec<u64>> = scenarios
        .iter()
        .map(|s| run(&s.runs[0].config).unwrap().hashes)
        .collect();
    tracing::subscriber::set_global_default(CountingSubscriber).unwrap();
    for (scenario, reference) in scenarios.iter().zip(&references) {
        assert_eq!(reference.len(), 16, "{}", scenario.name);
        for suite_run in &scenario.runs[1..] {
            let report = run(&suite_run.config).unwrap();
            assert_eq!(
                compare(reference, &report.hashes),
                Verdict::Match,
                "[{}] {}",
                scenario.name,
                suite_run.label
            );
            if suite_run.config.sample_stats {
                assert!(report.stats_executed.unwrap() > 0);
            }
        }
    }
    assert!(SPANS.load(Ordering::Relaxed) > 0, "graph tasks open spans");
}

#[test]
fn compare_reports_first_divergent_tick() {
    assert_eq!(compare(&[1, 2, 3], &[1, 2, 3]), Verdict::Match);
    assert_eq!(
        compare(&[1, 2, 3], &[1, 9, 3]),
        Verdict::Diverged { tick: 1 }
    );
    assert_eq!(compare(&[1, 2, 3], &[1, 2]), Verdict::Diverged { tick: 2 });
}
