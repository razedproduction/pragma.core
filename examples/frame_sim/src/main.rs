//! CLI вертикального среза `pragma.core`.
//!
//! ```text
//! frame_sim [--entities N] [--ticks N] [--workers N] [--apply-tick N] [--latency-ms N]
//!           [--verify W1,W2,...] [--mermaid]
//! ```
//!
//! `--verify` прогоняет сценарий на каждом числе воркеров дважды — с мгновенной и с
//! запаздывающей загрузкой — и сверяет хеши всех тиков. Расхождение — код выхода 1.
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::time::Duration;

use frame_sim::{SimConfig, SimReport, run};

/// Задержка «медленного диска» в режиме `--verify`: загрузка почти наверняка не успевает к
/// тику применения, и главный поток ждёт её между кадрами.
const SLOW_LOAD: Duration = Duration::from_millis(250);

struct Args {
    config: SimConfig,
    verify: Vec<usize>,
    mermaid: bool,
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
            "--mermaid" => args.mermaid = true,
            "--help" | "-h" => {
                return Err(String::from(
                    "usage: frame_sim [--entities N] [--ticks N] [--workers N] [--apply-tick N] \
                     [--latency-ms N] [--verify W1,W2,...] [--mermaid]",
                ));
            }
            other => return Err(format!("unknown argument `{other}` (see --help)")),
        }
    }
    Ok(args)
}

fn print_run(label: &str, report: &SimReport) {
    println!(
        "{label}: {} ticks in {:.2?}, load ready at tick {}, waited: {}, final hash {:#018x}",
        report.hashes.len(),
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
                    if let Some(tick) = ref_hashes
                        .iter()
                        .zip(&report.hashes)
                        .position(|(a, b)| a != b)
                    {
                        eprintln!("MISMATCH: `{label}` diverges from `{ref_label}` at tick {tick}");
                        ok = false;
                    } else if ref_hashes.len() != report.hashes.len() {
                        eprintln!("MISMATCH: `{label}` ran a different number of ticks");
                        ok = false;
                    }
                }
            }
        }
    }
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
    let result = if args.verify.is_empty() {
        run(&args.config).map(|report| {
            print_run(&format!("workers={}", args.config.workers), &report);
            true
        })
    } else {
        verify(&args.config, &args.verify)
    };
    match result {
        Ok(true) => {
            if !args.verify.is_empty() {
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
