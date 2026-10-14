//! The CLI of the `pragma.core` vertical slice.
//!
//! ```text
//! frame_sim [--entities N] [--ticks N] [--workers N] [--apply-tick N] [--latency-ms N]
//!           [--verify W1,W2,...] [--suite] [--mermaid]
//! ```
//!
//! `--verify` runs the scenario on each worker count twice, with an instant and with a
//! delayed load, and compares the hashes of all ticks. A mismatch means exit code 1.
//!
//! `--suite` is the mandatory determinism suite ([`frame_sim::determinism_suite`]): several
//! scenarios on 1, 2 and N workers (N is the cores minus the main thread, no oversubscription; the list
//! can be given with `--verify`) and environment variants, including a `tracing` subscriber.
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use frame_sim::{SimConfig, SimReport, Verdict, compare, default_workers, run};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// The "slow disk" delay in `--verify` mode: the load almost certainly misses the
/// apply tick, and the main thread waits for it between frames.
const SLOW_LOAD: Duration = Duration::from_millis(250);

struct Args {
    config: SimConfig,
    verify: Vec<usize>,
    suite: bool,
    mermaid: bool,
}

/// The determinism suite's subscriber: enables all spans and counts them (a check that
/// the instrumentation actually worked during the run).
struct CountingSubscriber;

static SPANS: AtomicU64 = AtomicU64::new(0);

impl Subscriber for CountingSubscriber {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(SPANS.fetch_add(1, Ordering::Relaxed).saturating_add(1))
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

fn parse_number<T: std::str::FromStr>(flag: &str, value: Option<String>) -> Result<T, String> {
    let value = value.ok_or_else(|| format!("{flag}: missing value"))?;
    value
        .parse()
        .map_err(|_| format!("{flag}: `{value}` is not a number"))
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        config: SimConfig::default(),
        verify: Vec::new(),
        suite: false,
        mermaid: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--entities" => args.config.entities = parse_number(&flag, it.next())?,
            "--ticks" => args.config.ticks = parse_number(&flag, it.next())?,
            "--workers" => args.config.workers = parse_number(&flag, it.next())?,
            "--apply-tick" => args.config.load_apply_tick = parse_number(&flag, it.next())?,
            "--latency-ms" => {
                args.config.load_latency = Duration::from_millis(parse_number(&flag, it.next())?);
            }
            "--verify" => {
                let list = it.next().ok_or("--verify: missing worker list")?;
                args.verify = list
                    .split(',')
                    .map(|w| parse_number("--verify", Some(w.trim().to_owned())))
                    .collect::<Result<_, _>>()?;
            }
            "--suite" => args.suite = true,
            "--mermaid" => args.mermaid = true,
            "--help" | "-h" => {
                return Err(String::from(
                    "usage: frame_sim [--entities N] [--ticks N] [--workers N] [--apply-tick N] \
                     [--latency-ms N] [--verify W1,W2,...] [--suite] [--mermaid]",
                ));
            }
            other => return Err(format!("unknown argument `{other}` (see --help)")),
        }
    }
    Ok(args)
}

fn print_run(label: &str, report: &SimReport) {
    let ticks = report.hashes.len();
    let ms_per_tick = report.elapsed.as_secs_f64() * 1000.0 / ticks.max(1) as f64;
    println!(
        "{label}: {ticks} ticks in {:.2?} ({ms_per_tick:.3} ms/tick), load ready at tick {}, \
         waited: {}, final hash {:#018x}",
        report.elapsed,
        report
            .load_ready_tick
            .map_or_else(|| String::from("-"), |t| t.to_string()),
        report.load_waited,
        report.final_hash().unwrap_or(0),
    );
}

fn verify(base: &SimConfig, workers: &[usize]) -> Result<bool, frame_sim::SimError> {
    let mut reference: Option<(String, Vec<u64>)> = None;
    let mut ok = true;
    for &w in workers {
        for latency in [Duration::ZERO, SLOW_LOAD] {
            let config = SimConfig {
                workers: w,
                load_latency: latency,
                ..base.clone()
            };
            let report = run(&config)?;
            let label = format!("workers={w} latency={}ms", latency.as_millis());
            print_run(&label, &report);
            match &reference {
                None => reference = Some((label, report.hashes)),
                Some((ref_label, ref_hashes)) => {
                    if let Verdict::Diverged { tick } = compare(ref_hashes, &report.hashes) {
                        eprintln!("MISMATCH: `{label}` diverges from `{ref_label}` at tick {tick}");
                        ok = false;
                    }
                }
            }
        }
    }
    Ok(ok)
}

/// The suite's default worker counts: 1, 2 and N without oversubscription.
fn suite_workers() -> Vec<usize> {
    let n = default_workers();
    let mut workers = vec![1, 2.min(n), n];
    workers.dedup();
    workers
}

/// The mandatory determinism suite. Runs with a `tracing` subscriber go in a second pass:
/// the global subscriber is installed once and never removed.
fn suite(base: &SimConfig, workers: &[usize]) -> Result<bool, frame_sim::SimError> {
    let scenarios = frame_sim::determinism_suite(base.ticks, base.entities, workers, SLOW_LOAD);
    let mut references: Vec<Option<Vec<u64>>> = vec![None; scenarios.len()];
    let mut ok = true;
    for traced_pass in [false, true] {
        if traced_pass && tracing::subscriber::set_global_default(CountingSubscriber).is_err() {
            eprintln!("error: a global tracing subscriber is already set");
            return Ok(false);
        }
        for (scenario, reference) in scenarios.iter().zip(references.iter_mut()) {
            for suite_run in scenario.runs.iter().filter(|r| r.traced == traced_pass) {
                let spans_before = SPANS.load(Ordering::Relaxed);
                let report = run(&suite_run.config)?;
                let label = format!("[{}] {}", scenario.name, suite_run.label);
                print_run(&label, &report);
                if suite_run.traced && SPANS.load(Ordering::Relaxed) == spans_before {
                    eprintln!("FAIL: `{label}` opened no tracing spans");
                    ok = false;
                }
                if suite_run.config.sample_stats && report.stats_executed.unwrap_or(0) == 0 {
                    eprintln!("FAIL: `{label}` saw no executed jobs in PoolStats");
                    ok = false;
                }
                match reference {
                    None => *reference = Some(report.hashes),
                    Some(hashes) => {
                        if let Verdict::Diverged { tick } = compare(hashes, &report.hashes) {
                            eprintln!(
                                "MISMATCH: `{label}` diverges from the scenario reference at \
                                 tick {tick}"
                            );
                            ok = false;
                        }
                    }
                }
            }
        }
    }
    let runs: usize = scenarios.iter().map(|s| s.runs.len()).sum();
    println!(
        "suite: {} scenarios, {runs} runs, workers {workers:?}",
        scenarios.len()
    );
    Ok(ok)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    if args.mermaid {
        match frame_sim::graph() {
            Ok(graph) => println!("{}", graph.to_mermaid()),
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    println!(
        "frame_sim: {} entities, {} ticks, load applied at tick {}",
        args.config.entities, args.config.ticks, args.config.load_apply_tick
    );
    let result = if args.suite {
        let workers = if args.verify.is_empty() {
            suite_workers()
        } else {
            args.verify.clone()
        };
        suite(&args.config, &workers)
    } else if args.verify.is_empty() {
        run(&args.config).map(|report| {
            print_run(&format!("workers={}", args.config.workers), &report);
            true
        })
    } else {
        verify(&args.config, &args.verify)
    };
    match result {
        Ok(true) => {
            if args.suite || !args.verify.is_empty() {
                println!("OK: hashes match on every tick");
            }
            ExitCode::SUCCESS
        }
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
