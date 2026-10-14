//! `frame_sim` is the vertical slice of `pragma.core` (milestone M3): a headless fixed-step
//! loop through every layer of the core.
//!
//! - **Pool** (`pragma_jobs`): every system parallelizes its work over the entities.
//! - **Frame graph** (`pragma_graph`): 9 systems with declared resource access.
//! - **Async** (`pragma_async`): background "loading" of a friction map: a synchronous "file read"
//!   in the blocking pool, decoding on a pool worker. The result is picked up by the
//!   [`AsyncHandle::poll_ready`] bridge at the start of a frame.
//! - **State hash** on every tick, also a graph system (a deterministic reduction).
//!
//! ## Determinism
//!
//! The tick hash depends only on the configuration, not on the number of workers nor on when
//! the load finished:
//!
//! - work over entities either does not depend on the split (element-wise) or is folded in chunk
//!   order (`parallel_reduce`), and the split depends only on the length;
//! - conflicting systems are ordered by the graph;
//! - the load is **applied exactly at tick `load_apply_tick`**: a result that is ready earlier
//!   waits in a buffer, and if it is not ready by that tick, the main thread waits for it **between
//!   frames** ([`AsyncHandle::wait`] outside pool work), not inside a frame task.
//!
//! ```
//! use frame_sim::{SimConfig, run};
//!
//! let config = SimConfig { entities: 2_000, ticks: 12, load_apply_tick: 4, ..SimConfig::default() };
//! let one = run(&SimConfig { workers: 1, ..config.clone() })?;
//! let many = run(&SimConfig { workers: 3, ..config })?;
//! assert_eq!(one.hashes, many.hashes);
//! # Ok::<(), frame_sim::SimError>(())
//! ```
#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use pragma_core::graph::{FrameGraph, GraphError, Resources, TaskContext};
use pragma_core::jobs::{
    DEFAULT_IDLE_SPIN, DEFAULT_WATCHDOG, Grain, JobError, JobsError, PoolConfig, ThreadPool,
};
use pragma_core::tasks::{AsyncConfig, AsyncError, AsyncHandle, Executor};

/// The side of the square world.
const WORLD: f32 = 1024.0;
/// The density and friction map grid: `GRID × GRID` cells.
const GRID: usize = 64;
const CELLS: usize = GRID * GRID;
const CELL: f32 = WORLD / GRID as f32;
const CENTER: f32 = WORLD / 2.0;
/// The fixed simulation step.
const DT: f32 = 1.0 / 60.0;
const PULL: f32 = 0.6;
const SWIRL: f32 = 40.0;
/// Friction while the map is not loaded.
const DEFAULT_FRICTION: f32 = 0.995;
const MAX_HEALTH: f32 = 100.0;
const HAZARD_DPS: f32 = 30.0;
const CROWD_LIMIT: f32 = 40.0;
const REGEN: f32 = 2.0;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Simulation errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SimError {
    /// Failed to create the pool.
    #[error("thread pool: {0}")]
    Pool(#[from] JobsError),
    /// A graph build or frame error.
    #[error("frame graph: {0}")]
    Graph(#[from] GraphError),
    /// A background load error.
    #[error("background load: {0}")]
    Async(#[from] AsyncError),
    /// A pool task error.
    #[error("job: {0}")]
    Job(#[from] JobError),
    /// Invalid configuration.
    #[error("invalid config: {0}")]
    Config(&'static str),
    /// A resource disappeared from the storage (a bug in the example).
    #[error("resource `{0}` is missing")]
    Missing(&'static str),
}

/// Run parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimConfig {
    /// Number of entities.
    pub entities: usize,
    /// Number of ticks.
    pub ticks: u64,
    /// Pool workers.
    pub workers: usize,
    /// The tick (zero-based) at the start of which the loaded friction map is applied.
    pub load_apply_tick: u64,
    /// An artificial "file read" delay in the blocking pool.
    pub load_latency: Duration,
    /// Seed of the initial state and the map.
    pub seed: u64,
    /// `yield` rounds before a worker parks ([`PoolConfig::with_idle_spin`]).
    pub idle_spin: u32,
    /// The watchdog threshold ([`PoolConfig::with_watchdog`]; effective in debug builds only).
    pub watchdog: Option<Duration>,
    /// Take a [`PoolStats`](pragma_core::jobs::PoolStats) snapshot after every frame.
    pub sample_stats: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            entities: 100_000,
            ticks: 300,
            workers: PoolConfig::default().workers(),
            load_apply_tick: 60,
            load_latency: Duration::ZERO,
            seed: 0x5EED_CAFE,
            idle_spin: DEFAULT_IDLE_SPIN,
            watchdog: Some(DEFAULT_WATCHDOG),
            sample_stats: false,
        }
    }
}

/// The outcome of a run.
#[derive(Debug, Clone)]
pub struct SimReport {
    /// The state hash after every tick.
    pub hashes: Vec<u64>,
    /// The tick at the start of which the bridge saw the load ready (depends on scheduling;
    /// for diagnostics only, not part of the hash).
    pub load_ready_tick: Option<u64>,
    /// The load was not ready by the apply tick, and the main thread waited for it between frames.
    pub load_waited: bool,
    /// Run time.
    pub elapsed: Duration,
    /// Pool tasks according to the last `PoolStats` snapshot (only with `sample_stats`; not part of
    /// the hash).
    pub stats_executed: Option<u64>,
}

impl SimReport {
    /// The hash after the last tick.
    #[must_use]
    pub fn final_hash(&self) -> Option<u64> {
        self.hashes.last().copied()
    }
}

#[derive(Clone, Copy)]
struct Body {
    pos: [f32; 2],
    vel: [f32; 2],
}

struct Clock {
    tick: u64,
}

struct Bodies(Vec<Body>);

struct Health(Vec<f32>);

struct Hazard {
    center: [f32; 2],
    radius: f32,
}

/// Per-cell friction map (the result of the background load); empty means the default friction.
struct Terrain {
    friction: Vec<f32>,
}

/// Number of entities per cell (by position after integration).
struct Grid(Vec<u32>);

struct Stats {
    total_health: f64,
    alive: u64,
    max_cell: u32,
}

struct StateHash(u64);

/// The grid cell index for a world point.
fn cell([x, y]: [f32; 2]) -> usize {
    let cx = ((x / CELL) as usize).min(GRID.saturating_sub(1));
    let cy = ((y / CELL) as usize).min(GRID.saturating_sub(1));
    cy.saturating_mul(GRID).saturating_add(cx)
}

/// splitmix64: a deterministic number stream for the initial state.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn unit(r: u64) -> f32 {
    (r >> 40) as f32 / (1_u64 << 24) as f32
}

fn fnv_word(hash: u64, word: u32) -> u64 {
    (hash ^ u64::from(word)).wrapping_mul(FNV_PRIME)
}

fn fnv_u64(hash: u64, value: u64) -> u64 {
    let low = (value & 0xFFFF_FFFF) as u32;
    let high = (value >> 32) as u32;
    fnv_word(fnv_word(hash, low), high)
}

fn initial_resources(config: &SimConfig) -> Resources {
    let mut rng = config.seed;
    let bodies = (0..config.entities)
        .map(|_| Body {
            pos: [
                unit(splitmix(&mut rng)) * WORLD,
                unit(splitmix(&mut rng)) * WORLD,
            ],
            vel: [
                (unit(splitmix(&mut rng)) - 0.5) * 20.0,
                (unit(splitmix(&mut rng)) - 0.5) * 20.0,
            ],
        })
        .collect();
    let mut resources = Resources::new();
    resources.insert(Clock { tick: 0 });
    resources.insert(Bodies(bodies));
    resources.insert(Health(vec![MAX_HEALTH; config.entities]));
    resources.insert(Hazard {
        center: [CENTER, CENTER],
        radius: 120.0,
    });
    resources.insert(Terrain {
        friction: Vec::new(),
    });
    resources.insert(Grid(vec![0; CELLS]));
    resources.insert(Stats {
        total_health: 0.0,
        alive: 0,
        max_cell: 0,
    });
    resources.insert(StateHash(FNV_OFFSET));
    resources
}

fn clock(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
    let mut clock = ctx.write::<Clock>()?;
    clock.tick = clock.tick.wrapping_add(1);
    Ok(())
}

/// The hazard zone moves in a circle around the center.
fn hazard(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
    let tick = ctx.read::<Clock>()?.tick;
    let angle = (tick % 3600) as f32 * 0.01;
    let mut hazard = ctx.write::<Hazard>()?;
    hazard.center = [CENTER + 300.0 * angle.cos(), CENTER + 300.0 * angle.sin()];
    Ok(())
}

/// Pull towards the center + swirl, map friction, slowdown in crowds (the previous tick's grid).
fn forces(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
    let terrain = ctx.read::<Terrain>()?;
    let grid = ctx.read::<Grid>()?;
    let mut bodies = ctx.write::<Bodies>()?;
    let (terrain, grid): (&Terrain, &Grid) = (&terrain, &grid);
    ctx.pool()
        .parallel_for(&mut bodies.0, Grain::Auto, |body| {
            let c = cell(body.pos);
            let friction = terrain.friction.get(c).copied().unwrap_or(DEFAULT_FRICTION);
            let density = grid.0.get(c).copied().unwrap_or(0) as f32;
            let crowd = 1.0 / (1.0 + density * 0.002);
            let [px, py] = body.pos;
            let (dx, dy) = (CENTER - px, CENTER - py);
            let len = (dx * dx + dy * dy).sqrt().max(1.0);
            let ax = dx * PULL - dy / len * SWIRL;
            let ay = dy * PULL + dx / len * SWIRL;
            let [vx, vy] = body.vel;
            body.vel = [
                (vx + ax * DT) * friction * crowd,
                (vy + ay * DT) * friction * crowd,
            ];
        })?;
    Ok(())
}

fn reflect(p: f32, v: f32) -> (f32, f32) {
    if p < 0.0 {
        ((-p).min(WORLD - 0.001), -v)
    } else if p >= WORLD {
        ((2.0 * WORLD - p).clamp(0.0, WORLD - 0.001), -v)
    } else {
        (p, v)
    }
}

/// Integration and reflection off the world boundaries.
fn integrate(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
    let mut bodies = ctx.write::<Bodies>()?;
    ctx.pool()
        .parallel_for(&mut bodies.0, Grain::Auto, |body| {
            let [px, py] = body.pos;
            let [vx, vy] = body.vel;
            let (x, vx) = reflect(px + vx * DT, vx);
            let (y, vy) = reflect(py + vy * DT, vy);
            body.pos = [x, y];
            body.vel = [vx, vy];
        })?;
    Ok(())
}

/// Per-cell density: chunk histograms are summed in chunk order.
fn grid(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
    let bodies = ctx.read::<Bodies>()?;
    let counts = ctx.pool().parallel_reduce(
        &bodies.0,
        Grain::Auto,
        |_, chunk| {
            let mut histogram = vec![0_u32; CELLS];
            for body in chunk {
                if let Some(count) = histogram.get_mut(cell(body.pos)) {
                    *count = count.saturating_add(1);
                }
            }
            histogram
        },
        |mut acc, part| {
            for (a, p) in acc.iter_mut().zip(part) {
                *a = a.saturating_add(p);
            }
            acc
        },
    )?;
    let mut grid = ctx.write::<Grid>()?;
    match counts {
        Some(counts) => grid.0 = counts,
        None => grid.0.fill(0),
    }
    Ok(())
}

/// Damage in the hazard zone and in overcrowded cells.
fn damage(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
    let bodies = ctx.read::<Bodies>()?;
    let grid = ctx.read::<Grid>()?;
    let hazard = ctx.read::<Hazard>()?;
    let mut health = ctx.write::<Health>()?;
    let (bodies, grid, hazard): (&Bodies, &Grid, &Hazard) = (&bodies, &grid, &hazard);
    let r2 = hazard.radius * hazard.radius;
    ctx.pool()
        .parallel_for_chunks(&mut health.0, Grain::Auto, |offset, chunk| {
            let positions = bodies.0.get(offset..).unwrap_or_default();
            for (h, body) in chunk.iter_mut().zip(positions) {
                if *h <= 0.0 {
                    continue;
                }
                let [x, y] = body.pos;
                let (dx, dy) = (x - hazard.center[0], y - hazard.center[1]);
                let mut dmg = if dx * dx + dy * dy < r2 {
                    HAZARD_DPS * DT
                } else {
                    0.0
                };
                let density = grid.0.get(cell(body.pos)).copied().unwrap_or(0) as f32;
                if density > CROWD_LIMIT {
                    dmg += (density - CROWD_LIMIT) * 0.01 * DT;
                }
                *h -= dmg;
            }
        })?;
    Ok(())
}

/// Health regeneration of living entities.
fn regen(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
    let mut health = ctx.write::<Health>()?;
    ctx.pool().parallel_for(&mut health.0, Grain::Auto, |h| {
        if *h > 0.0 {
            *h = (*h + REGEN * DT).min(MAX_HEALTH);
        }
    })?;
    Ok(())
}

/// Summary: total health (f64, folded in chunk order), alive count, peak density.
fn stats(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
    let health = ctx.read::<Health>()?;
    let grid = ctx.read::<Grid>()?;
    let (total_health, alive) = ctx
        .pool()
        .parallel_reduce(
            &health.0,
            Grain::Auto,
            |_, chunk| {
                chunk.iter().fold((0.0_f64, 0_u64), |(sum, alive), &h| {
                    if h > 0.0 {
                        (sum + f64::from(h), alive.saturating_add(1))
                    } else {
                        (sum, alive)
                    }
                })
            },
            |(s1, a1), (s2, a2)| (s1 + s2, a1.saturating_add(a2)),
        )?
        .unwrap_or((0.0, 0));
    let max_cell = grid.0.iter().copied().max().unwrap_or(0);
    let mut stats = ctx.write::<Stats>()?;
    *stats = Stats {
        total_health,
        alive,
        max_cell,
    };
    Ok(())
}

/// The hash of the whole state: FNV-1a over 32-bit words within a chunk, chunk hashes combined in
/// chunk order.
fn hash(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
    let tick = ctx.read::<Clock>()?.tick;
    let bodies = ctx.read::<Bodies>()?;
    let health = ctx.read::<Health>()?;
    let hazard = ctx.read::<Hazard>()?;
    let stats = ctx.read::<Stats>()?;
    let health_all: &[f32] = &health.0;
    let entities = ctx
        .pool()
        .parallel_reduce(
            &bodies.0,
            Grain::Auto,
            |offset, chunk| {
                let healths = health_all.get(offset..).unwrap_or_default();
                chunk
                    .iter()
                    .zip(healths)
                    .fold(FNV_OFFSET, |h, (body, &hp)| {
                        let [px, py] = body.pos;
                        let [vx, vy] = body.vel;
                        [px, py, vx, vy, hp]
                            .iter()
                            .fold(h, |h, v| fnv_word(h, v.to_bits()))
                    })
            },
            fnv_u64,
        )?
        .unwrap_or(FNV_OFFSET);
    let [hx, hy] = hazard.center;
    let mut h = fnv_u64(FNV_OFFSET, tick);
    h = fnv_u64(h, entities);
    h = fnv_word(fnv_word(h, hx.to_bits()), hy.to_bits());
    h = fnv_u64(h, stats.total_health.to_bits());
    h = fnv_u64(h, stats.alive);
    h = fnv_word(h, stats.max_cell);
    ctx.write::<StateHash>()?.0 = h;
    Ok(())
}

fn build_graph() -> Result<FrameGraph, GraphError> {
    let mut g = FrameGraph::builder();
    g.task("clock", |t| t.writes::<Clock>(), clock);
    g.task(
        "hazard",
        |t| t.reads::<Clock>().writes::<Hazard>().after("clock"),
        hazard,
    );
    g.task(
        "forces",
        |t| t.reads::<Terrain>().reads::<Grid>().writes::<Bodies>(),
        forces,
    );
    g.task(
        "integrate",
        |t| t.writes::<Bodies>().after("forces"),
        integrate,
    );
    g.task(
        "grid",
        |t| t.reads::<Bodies>().writes::<Grid>().after("integrate"),
        grid,
    );
    g.task(
        "damage",
        |t| {
            t.reads::<Bodies>()
                .reads::<Grid>()
                .reads::<Hazard>()
                .writes::<Health>()
                .after("grid")
                .after("hazard")
        },
        damage,
    );
    g.task("regen", |t| t.writes::<Health>().after("damage"), regen);
    g.task(
        "stats",
        |t| {
            t.reads::<Health>()
                .reads::<Grid>()
                .writes::<Stats>()
                .after("regen")
        },
        stats,
    );
    g.task(
        "hash",
        |t| {
            t.reads::<Clock>()
                .reads::<Bodies>()
                .reads::<Health>()
                .reads::<Hazard>()
                .reads::<Stats>()
                .writes::<StateHash>()
                .after("stats")
        },
        hash,
    );
    g.build()
}

/// The graph of simulation systems (for debugging: `to_mermaid()`/`to_dot()`).
pub fn graph() -> Result<FrameGraph, SimError> {
    Ok(build_graph()?)
}

/// A synthetic friction map "file": one byte per cell.
fn read_terrain_file(seed: u64, latency: Duration) -> Vec<u8> {
    if !latency.is_zero() {
        std::thread::sleep(latency);
    }
    let mut rng = seed ^ 0x7E44_A1B0_0000_0001;
    (0..CELLS)
        .map(|_| (splitmix(&mut rng) >> 56) as u8)
        .collect()
}

/// "Decoding" the map: byte → friction in `[0.985, 0.999]`.
fn decode_terrain(raw: &[u8]) -> Terrain {
    Terrain {
        friction: raw
            .iter()
            .map(|&b| 0.985 + f32::from(b) / 255.0 * 0.014)
            .collect(),
    }
}

/// Background loading: the synchronous read in the blocking pool, decoding on a pool worker.
fn start_load(
    exec: &Executor,
    seed: u64,
    latency: Duration,
) -> AsyncHandle<Result<Terrain, AsyncError>> {
    let io = exec.clone();
    exec.spawn_async(async move {
        let raw = io.unblock(move || read_terrain_file(seed, latency)).await?;
        Ok(decode_terrain(&raw))
    })
}

/// Runs the simulation and returns the state hash after every tick.
pub fn run(config: &SimConfig) -> Result<SimReport, SimError> {
    if config.ticks == 0 {
        return Err(SimError::Config("ticks must be >= 1"));
    }
    let started = Instant::now();
    let pool = ThreadPool::new(
        PoolConfig::default()
            .with_workers(config.workers)
            .with_idle_spin(config.idle_spin)
            .with_watchdog(config.watchdog),
    )?;
    let exec = Executor::new(&pool, AsyncConfig::default().with_blocking_threads(1))?;
    let mut graph = build_graph()?;
    let mut resources = initial_resources(config);

    let mut loading = Some(start_load(&exec, config.seed, config.load_latency));
    let mut staged: Option<Terrain> = None;
    let mut load_ready_tick = None;
    let mut load_waited = false;
    let mut stats_executed = None;
    let mut hashes = Vec::with_capacity(usize::try_from(config.ticks).unwrap_or(0));

    for tick in 0..config.ticks {
        // The "async → frame" bridge: a non-blocking check at the start of the frame. A ready result
        // waits in a buffer until the apply tick, so the moment it became ready does not affect the state.
        if let Some(handle) = loading.as_mut()
            && let Some(result) = handle.poll_ready()
        {
            staged = Some(result??);
            load_ready_tick = Some(tick);
            loading = None;
        }
        if tick == config.load_apply_tick {
            let terrain = match staged.take() {
                Some(terrain) => terrain,
                None => {
                    // Not ready in time: wait between frames, outside pool work.
                    load_waited = true;
                    let handle = loading.take().ok_or(SimError::Missing("terrain load"))?;
                    handle.wait()??
                }
            };
            *resources
                .get_mut::<Terrain>()
                .ok_or(SimError::Missing("Terrain"))? = terrain;
        }
        graph.run(&pool, &mut resources)?;
        let hash = resources
            .get::<StateHash>()
            .map(|h| h.0)
            .ok_or(SimError::Missing("StateHash"))?;
        hashes.push(hash);
        if config.sample_stats {
            stats_executed = Some(pool.stats().executed());
        }
    }
    // A load that was never picked up (the apply tick lies beyond the run) is cancelled by dropping the handle.
    drop(loading);
    Ok(SimReport {
        hashes,
        load_ready_tick,
        load_waited,
        elapsed: started.elapsed(),
        stats_executed,
    })
}

/// One run of the determinism suite.
#[derive(Debug, Clone)]
pub struct SuiteRun {
    /// A label for the report.
    pub label: String,
    /// The run configuration.
    pub config: SimConfig,
    /// A run with a `tracing` subscriber: such runs execute after the others, once the
    /// global subscriber has been installed.
    pub traced: bool,
}

/// A suite scenario: all runs must produce the same hash on every tick.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// The scenario name.
    pub name: &'static str,
    /// The runs; the first one is the reference.
    pub runs: Vec<SuiteRun>,
}

/// The mandatory determinism suite (milestone M5).
///
/// Scenarios differ in seed, number of entities (including an empty world and an odd count) and the
/// load apply tick (before the load is ready, on the first tick, beyond the run). Each
/// scenario runs on every worker count from `workers` with an instant and a delayed
/// (`slow_load`) load, and on the largest worker count also with environment variants
/// that must not affect the hash: `idle_spin` 0 and 128, `PoolStats` snapshots after every
/// frame, the watchdog disabled and with a 1 ms threshold (it fires in a debug build), and a
/// `tracing` subscriber (`traced`). The number of entities is capped by `max_entities` so that the
/// same suite works for a debug test.
#[must_use]
pub fn determinism_suite(
    ticks: u64,
    max_entities: usize,
    workers: &[usize],
    slow_load: Duration,
) -> Vec<Scenario> {
    struct Shape {
        name: &'static str,
        entities: usize,
        seed: u64,
        apply: u64,
    }
    let shapes = [
        Shape {
            name: "default",
            entities: 100_000,
            seed: 0x5EED_CAFE,
            apply: 60,
        },
        Shape {
            name: "odd-count",
            entities: 33_333,
            seed: 7,
            apply: ticks / 2,
        },
        Shape {
            name: "apply-at-start",
            entities: 20_000,
            seed: 0x1234_5678_9ABC_DEF0,
            apply: 0,
        },
        Shape {
            name: "never-applied",
            entities: 50_000,
            seed: 42,
            apply: u64::MAX,
        },
        Shape {
            name: "empty-world",
            entities: 0,
            seed: 1,
            apply: 1,
        },
    ];
    let widest = workers.iter().copied().max().unwrap_or(1);
    shapes
        .iter()
        .map(|shape| {
            let base = SimConfig {
                entities: shape.entities.min(max_entities),
                ticks,
                load_apply_tick: shape.apply,
                seed: shape.seed,
                ..SimConfig::default()
            };
            let mut runs = Vec::new();
            for &w in workers {
                for latency in [Duration::ZERO, slow_load] {
                    runs.push(SuiteRun {
                        label: format!("workers={w} latency={}ms", latency.as_millis()),
                        config: SimConfig {
                            workers: w,
                            load_latency: latency,
                            ..base.clone()
                        },
                        traced: false,
                    });
                }
            }
            let wide = SimConfig {
                workers: widest,
                ..base
            };
            let variants = [
                (
                    "idle_spin=0",
                    false,
                    SimConfig {
                        idle_spin: 0,
                        ..wide.clone()
                    },
                ),
                (
                    "idle_spin=128",
                    false,
                    SimConfig {
                        idle_spin: 128,
                        ..wide.clone()
                    },
                ),
                (
                    "pool-stats",
                    false,
                    SimConfig {
                        sample_stats: true,
                        ..wide.clone()
                    },
                ),
                (
                    "watchdog=off",
                    false,
                    SimConfig {
                        watchdog: None,
                        ..wide.clone()
                    },
                ),
                (
                    "watchdog=1ms",
                    false,
                    SimConfig {
                        watchdog: Some(Duration::from_millis(1)),
                        ..wide.clone()
                    },
                ),
                (
                    "tracing",
                    true,
                    SimConfig {
                        load_latency: slow_load,
                        ..wide
                    },
                ),
            ];
            runs.extend(variants.into_iter().map(|(name, traced, config)| SuiteRun {
                label: format!("workers={widest} {name}"),
                config,
                traced,
            }));
            Scenario {
                name: shape.name,
                runs,
            }
        })
        .collect()
}

/// The outcome of checking a scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The hashes of all ticks matched the reference.
    Match,
    /// A divergence at tick `tick` (or a different number of ticks: `tick` is the shorter length).
    Diverged {
        /// The first tick with a different hash.
        tick: usize,
    },
}

/// Compares a run's hashes with the reference.
#[must_use]
pub fn compare(reference: &[u64], hashes: &[u64]) -> Verdict {
    match reference.iter().zip(hashes).position(|(a, b)| a != b) {
        Some(tick) => Verdict::Diverged { tick },
        None if reference.len() != hashes.len() => Verdict::Diverged {
            tick: reference.len().min(hashes.len()),
        },
        None => Verdict::Match,
    }
}

/// The worker count without oversubscription: logical cores minus the main thread (at least 1).
#[must_use]
pub fn default_workers() -> usize {
    PoolConfig::default().workers()
}
