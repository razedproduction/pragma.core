//! L3: the frame graph of `pragma.core`.
//!
//! A task declares which resources it reads and writes and, if needed, an explicit order
//! (`before`/`after`). [`FrameGraphBuilder::build`] validates the description once: cycles,
//! unknown dependencies and **unordered conflicts** (one task writes what another one
//! reads or writes) are a build error rather than a race in the frame. [`FrameGraph::run`] runs
//! the tasks on a `pragma_jobs` pool every frame: independent ones in parallel, with no allocations
//! (`docs/architecture.md` §3.3).
//!
//! Resources live in [`Resources`], one value per type. The graph does not own them,
//! so a single graph can run over different instances of the state.
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
//!     // Nested parallelism inside a graph task.
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
//! // An unordered conflict is caught at build time.
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
pub use plan::{MAX_EDGES, MAX_TASKS};
pub use resources::{Res, ResMut, Resources};
