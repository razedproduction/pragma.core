//! `frame_sim` — вертикальный срез `pragma.core` (веха M3): headless-цикл с фиксированным
//! шагом через все слои ядра.
//!
//! - **Пул** (`pragma_jobs`): каждая система распараллеливает работу над сущностями.
//! - **Граф кадра** (`pragma_graph`): 9 систем с объявленным доступом к ресурсам.
//! - **Async** (`pragma_async`): фоновая «загрузка» карты трения — синхронное «чтение файла»
//!   в blocking-пуле, декодирование на воркере пула. Результат забирается мостом
//!   [`AsyncHandle::poll_ready`] в начале кадра.
//! - **Хеш состояния** на каждом тике — тоже система графа (детерминированная редукция).
//!
//! ## Детерминизм
//!
//! Хеш тика зависит только от конфигурации, а не от числа воркеров и не от того, когда
//! закончилась загрузка:
//!
//! - работа над сущностями не зависит от разбиения (поэлементная) или сворачивается в порядке
//!   чанков (`parallel_reduce`), а разбиение зависит только от длины;
//! - конфликтующие системы упорядочены графом;
//! - загрузка **применяется ровно на тике `load_apply_tick`**: готовый раньше результат
//!   ждёт в буфере, а если к этому тику он не готов, главный поток дожидается его **между
//!   кадрами** ([`AsyncHandle::wait`] вне работы пула), а не внутри задачи кадра.
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
use pragma_core::jobs::{Grain, JobError, JobsError, PoolConfig, ThreadPool};
use pragma_core::tasks::{AsyncConfig, AsyncError, AsyncHandle, Executor};

/// Сторона квадратного мира.
const WORLD: f32 = 1024.0;
/// Сетка плотности и карты трения: `GRID × GRID` клеток.
const GRID: usize = 64;
const CELLS: usize = GRID * GRID;
const CELL: f32 = WORLD / GRID as f32;
const CENTER: f32 = WORLD / 2.0;
/// Фиксированный шаг симуляции.
const DT: f32 = 1.0 / 60.0;
const PULL: f32 = 0.6;
const SWIRL: f32 = 40.0;
/// Трение, пока карта не загружена.
const DEFAULT_FRICTION: f32 = 0.995;
const MAX_HEALTH: f32 = 100.0;
const HAZARD_DPS: f32 = 30.0;
const CROWD_LIMIT: f32 = 40.0;
const REGEN: f32 = 2.0;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Ошибки симуляции.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SimError {
    /// Не удалось создать пул.
    #[error("thread pool: {0}")]
    Pool(#[from] JobsError),
    /// Ошибка сборки или кадра графа.
    #[error("frame graph: {0}")]
    Graph(#[from] GraphError),
    /// Ошибка фоновой загрузки.
    #[error("background load: {0}")]
    Async(#[from] AsyncError),
    /// Ошибка задачи пула.
    #[error("job: {0}")]
    Job(#[from] JobError),
    /// Некорректная конфигурация.
    #[error("invalid config: {0}")]
    Config(&'static str),
    /// Ресурс пропал из хранилища (баг примера).
    #[error("resource `{0}` is missing")]
    Missing(&'static str),
}

/// Параметры прогона.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimConfig {
    /// Число сущностей.
    pub entities: usize,
    /// Число тиков.
    pub ticks: u64,
    /// Воркеры пула.
    pub workers: usize,
    /// Тик (с нуля), в начале которого применяется загруженная карта трения.
    pub load_apply_tick: u64,
    /// Искусственная задержка «чтения файла» в blocking-пуле.
    pub load_latency: Duration,
    /// Seed начального состояния и карты.
    pub seed: u64,
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
        }
    }
}

/// Итог прогона.
#[derive(Debug, Clone)]
pub struct SimReport {
    /// Хеш состояния после каждого тика.
    pub hashes: Vec<u64>,
    /// Тик, в начале которого мост увидел готовую загрузку (зависит от расписания —
    /// только для диагностики, в хеш не входит).
    pub load_ready_tick: Option<u64>,
    /// К тику применения загрузка не была готова и главный поток ждал её между кадрами.
    pub load_waited: bool,
    /// Время прогона.
    pub elapsed: Duration,
}

impl SimReport {
    /// Хеш после последнего тика.
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

/// Карта трения по клеткам (результат фоновой загрузки); пустая — трение по умолчанию.
struct Terrain {
    friction: Vec<f32>,
}

/// Число сущностей в клетке (по положению после интегрирования).
struct Grid(Vec<u32>);

struct Stats {
    total_health: f64,
    alive: u64,
    max_cell: u32,
}

struct StateHash(u64);

/// Индекс клетки сетки для точки мира.
fn cell([x, y]: [f32; 2]) -> usize {
    let cx = ((x / CELL) as usize).min(GRID.saturating_sub(1));
    let cy = ((y / CELL) as usize).min(GRID.saturating_sub(1));
    cy.saturating_mul(GRID).saturating_add(cx)
}

/// splitmix64: детерминированный поток чисел для начального состояния.
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

/// Опасная зона движется по окружности вокруг центра.
fn hazard(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
    let tick = ctx.read::<Clock>()?.tick;
    let angle = (tick % 3600) as f32 * 0.01;
    let mut hazard = ctx.write::<Hazard>()?;
    hazard.center = [CENTER + 300.0 * angle.cos(), CENTER + 300.0 * angle.sin()];
    Ok(())
}

/// Притяжение к центру + вихрь, трение по карте, торможение в толпе (сетка прошлого тика).
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

/// Интегрирование и отражение от границ мира.
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

/// Плотность по клеткам: гистограммы чанков складываются в порядке чанков.
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

/// Урон в опасной зоне и в переполненных клетках.
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

/// Восстановление здоровья живых сущностей.
fn regen(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
    let mut health = ctx.write::<Health>()?;
    ctx.pool().parallel_for(&mut health.0, Grain::Auto, |h| {
        if *h > 0.0 {
            *h = (*h + REGEN * DT).min(MAX_HEALTH);
        }
    })?;
    Ok(())
}

/// Сводка: суммарное здоровье (f64, свёртка в порядке чанков), живые, пик плотности.
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

/// Хеш всего состояния: FNV-1a по 32-битным словам внутри чанка, хеши чанков — в порядке
/// чанков.
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

/// Граф систем симуляции (для отладки: `to_mermaid()`/`to_dot()`).
pub fn graph() -> Result<FrameGraph, SimError> {
    Ok(build_graph()?)
}

/// Синтетический «файл» карты трения: байт на клетку.
fn read_terrain_file(seed: u64, latency: Duration) -> Vec<u8> {
    if !latency.is_zero() {
        std::thread::sleep(latency);
    }
    let mut rng = seed ^ 0x7E44_A1B0_0000_0001;
    (0..CELLS)
        .map(|_| (splitmix(&mut rng) >> 56) as u8)
        .collect()
}

/// «Декодирование» карты: байт → трение в `[0.985, 0.999]`.
fn decode_terrain(raw: &[u8]) -> Terrain {
    Terrain {
        friction: raw
            .iter()
            .map(|&b| 0.985 + f32::from(b) / 255.0 * 0.014)
            .collect(),
    }
}

/// Фоновая загрузка: синхронное чтение — в blocking-пуле, декодирование — на воркере пула.
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

/// Прогоняет симуляцию и возвращает хеш состояния после каждого тика.
pub fn run(config: &SimConfig) -> Result<SimReport, SimError> {
    if config.ticks == 0 {
        return Err(SimError::Config("ticks must be >= 1"));
    }
    let started = Instant::now();
    let pool = ThreadPool::new(PoolConfig::default().with_workers(config.workers))?;
    let exec = Executor::new(&pool, AsyncConfig::default().with_blocking_threads(1))?;
    let mut graph = build_graph()?;
    let mut resources = initial_resources(config);

    let mut loading = Some(start_load(&exec, config.seed, config.load_latency));
    let mut staged: Option<Terrain> = None;
    let mut load_ready_tick = None;
    let mut load_waited = false;
    let mut hashes = Vec::with_capacity(usize::try_from(config.ticks).unwrap_or(0));

    for tick in 0..config.ticks {
        // Мост «async → кадр»: неблокирующая проверка в начале кадра. Готовый результат
        // ждёт в буфере до тика применения — момент готовности на состояние не влияет.
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
                    // Не успела: ждём между кадрами, вне работы пула.
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
    }
    // Незабранная загрузка (тик применения за пределами прогона) отменяется сбросом хэндла.
    drop(loading);
    Ok(SimReport {
        hashes,
        load_ready_tick,
        load_waited,
        elapsed: started.elapsed(),
    })
}
