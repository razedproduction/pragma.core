//! Обязательный набор детерминизма в debug-сборке: здесь watchdog реально работает (порог
//! 1 мс срабатывает), а подписчик `tracing` включает span на каждую задачу — хеш не меняется.
//! Тот же набор в release на полном размере — `frame_sim --suite` в CI.
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

/// Одна функция-тест: подписчик глобальный. Все прогоны — с подписчиком, кроме эталона.
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
