//! Хеш каждого тика не зависит от числа воркеров и от момента завершения загрузки.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::time::Duration;

use frame_sim::{SimConfig, run};

fn small() -> SimConfig {
    SimConfig {
        entities: 20_000,
        ticks: 40,
        load_apply_tick: 10,
        ..SimConfig::default()
    }
}

#[test]
fn hashes_match_across_workers_and_load_timing() {
    let base = small();
    let reference = run(&SimConfig {
        workers: 1,
        ..base.clone()
    })
    .unwrap();
    assert_eq!(reference.hashes.len(), 40);
    for workers in [1, 2, 4] {
        for latency in [Duration::ZERO, Duration::from_millis(60)] {
            let report = run(&SimConfig {
                workers,
                load_latency: latency,
                ..base.clone()
            })
            .unwrap();
            assert_eq!(
                report.hashes, reference.hashes,
                "workers={workers} latency={latency:?}"
            );
        }
    }
}

#[test]
fn background_load_affects_state_from_apply_tick() {
    let base = small();
    let applied = run(&base).unwrap();
    let never = run(&SimConfig {
        load_apply_tick: u64::MAX,
        ..base
    })
    .unwrap();
    // До тика применения состояния совпадают, после — расходятся: загрузка реально участвует.
    assert_eq!(applied.hashes[..10], never.hashes[..10]);
    assert_ne!(applied.hashes[10..], never.hashes[10..]);
}

#[test]
fn zero_ticks_is_an_error() {
    assert!(
        run(&SimConfig {
            ticks: 0,
            ..small()
        })
        .is_err()
    );
}
