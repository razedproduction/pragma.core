//! L3: граф задач кадра `pragma.core`.
//!
//! Задача объявляет, какие ресурсы читает и пишет, и, при необходимости, явный порядок
//! (`before`/`after`). [`FrameGraphBuilder::build`] один раз проверяет описание: циклы,
//! неизвестные зависимости и **конфликты без порядка** (одна задача пишет то, что другая
//! читает или пишет) — ошибка сборки, а не гонка в кадре. [`FrameGraph::run`] каждый кадр
//! исполняет задачи на пуле `pragma_jobs`: независимые — параллельно, без аллокаций
//! (`docs/architecture.md` §3.3).
//!
//! Ресурсы лежат в [`Resources`] — по одному значению каждого типа. Граф ими не владеет,
//! поэтому один граф можно исполнять над разными экземплярами состояния.
//!
//! ```
//! use pragma_graph::{FrameGraph, GraphError, Resources, TaskContext};
//! use pragma_jobs::{Grain, PoolConfig, ThreadPool};
//!
//! struct Input(f32);
//! struct Bodies(Vec<f32>);
//! struct Energy(f32);
//!
//! fn physics(ctx: &TaskContext<'_>) -> Result<(), GraphError> {
//!     let dt = ctx.read::<Input>()?.0;
//!     let mut bodies = ctx.write::<Bodies>()?;
//!     // Вложенный параллелизм внутри задачи графа.
//!     ctx.pool().parallel_for(&mut bodies.0, Grain::Auto, |x| *x += dt)?;
//!     Ok(())
//! }
//!
//! let mut g = FrameGraph::builder();
//! g.task("input", |t| t.writes::<Input>(), |ctx| -> Result<(), GraphError> {
//!     ctx.write::<Input>()?.0 = 0.5;
//!     Ok(())
//! });
//! g.task("physics", |t| t.reads::<Input>().writes::<Bodies>().after("input"), physics);
//! g.task("energy", |t| t.reads::<Bodies>().writes::<Energy>().after("physics"), |ctx| -> Result<(), GraphError> {
//!     let sum = ctx.read::<Bodies>()?.0.iter().sum();
//!     ctx.write::<Energy>()?.0 = sum;
//!     Ok(())
//! });
//! let mut graph = g.build()?;
//! assert_eq!(graph.order().collect::<Vec<_>>(), ["input", "physics", "energy"]);
//!
//! let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
//! let mut resources = Resources::new();
//! resources.insert(Input(0.0));
//! resources.insert(Bodies(vec![0.0; 1000]));
//! resources.insert(Energy(0.0));
//! graph.run(&pool, &mut resources)?;
//! assert_eq!(resources.get::<Energy>().map(|e| e.0), Some(500.0));
//!
//! // Конфликт без порядка ловится при сборке.
//! let mut g = FrameGraph::builder();
//! g.task("a", |t| t.writes::<Bodies>(), |_| ());
//! g.task("b", |t| t.reads::<Bodies>(), |_| ());
//! assert!(matches!(g.build(), Err(GraphError::AmbiguousOrder { .. })));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![forbid(unsafe_code)]

mod context;
mod error;
mod export;
mod graph;
mod plan;
mod resources;

pub use context::{TaskContext, TaskResult};
pub use error::GraphError;
pub use graph::{FrameGraph, FrameGraphBuilder, TaskConfig};
pub use resources::{Res, ResMut, Resources};
