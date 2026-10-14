/// Task priority (`docs/architecture.md` §3.2). `Ord` order: `Frame` is the most urgent.
///
/// Priority is chosen explicitly only for [`ThreadPool::spawn`](crate::ThreadPool::spawn) and
/// [`Spawner::spawn_detached`](crate::Spawner::spawn_detached); `scope`/`join`/`parallel_*`
/// inherit it from the calling task (see [`ThreadPool::scope`](crate::ThreadPool::scope)).
///
/// ```
/// use pragma_jobs::{PoolConfig, Priority, ThreadPool, current_priority};
///
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
/// let streaming = pool.spawn(Priority::Background, current_priority);
/// assert_eq!(streaming.join()?, Some(Priority::Background));
///
/// assert!(Priority::Frame < Priority::Background);
/// assert_eq!(Priority::Background.as_str(), "background");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[non_exhaustive]
pub enum Priority {
    /// Work of the current frame (graph, `parallel_for`): taken first.
    Frame,
    /// Tasks without a frame deadline: after all `Frame` tasks.
    #[default]
    Normal,
    /// Baking, decompression, streaming: a separate global queue and a limit on
    /// simultaneously busy workers ([`PoolConfig::with_background_limit`]).
    ///
    /// [`PoolConfig::with_background_limit`]: crate::PoolConfig::with_background_limit
    Background,
}

impl Priority {
    /// The priority name for logs and metrics: `"frame"`, `"normal"`, `"background"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Frame => "frame",
            Self::Normal => "normal",
            Self::Background => "background",
        }
    }
}

/// How data is split into tasks in `parallel_*`.
///
/// The split depends **only** on the data length and `Grain`, not on the number of workers, so
/// `parallel_reduce` gives a bit-identical result on any pool (`architecture.md` §6).
///
/// `Auto` suits uniform, cheap per-element work; pick `Fixed` when elements are expensive (smaller
/// chunks balance better) or when the chunk boundaries must stay put while the data grows.
///
/// ```
/// use pragma_jobs::Grain;
///
/// assert_eq!(Grain::Auto.chunk_size(1_000_000), 7813); // ~128 chunks
/// assert_eq!(Grain::Auto.chunk_size(100), Grain::AUTO_MIN_CHUNK); // small input: one chunk
/// assert_eq!(Grain::Fixed(256).chunk_size(1_000_000), 256);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Grain {
    /// About [`Grain::AUTO_CHUNKS`] chunks, but no fewer than [`Grain::AUTO_MIN_CHUNK`] elements per chunk.
    #[default]
    Auto,
    /// A fixed chunk size in elements (`0` is treated as `1`).
    Fixed(usize),
}

impl Grain {
    /// Target number of chunks for [`Grain::Auto`].
    pub const AUTO_CHUNKS: usize = 128;
    /// Minimum chunk size for [`Grain::Auto`]: below it the overhead costs more than the work.
    pub const AUTO_MIN_CHUNK: usize = 64;

    /// The chunk size for `len` elements (always `>= 1`).
    #[must_use]
    pub fn chunk_size(self, len: usize) -> usize {
        match self {
            Self::Auto => len.div_ceil(Self::AUTO_CHUNKS).max(Self::AUTO_MIN_CHUNK),
            Self::Fixed(n) => n.max(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_size_is_positive_and_worker_independent() {
        assert_eq!(Grain::Auto.chunk_size(0), Grain::AUTO_MIN_CHUNK);
        assert_eq!(Grain::Auto.chunk_size(1_000_000), 7813);
        assert_eq!(Grain::Fixed(0).chunk_size(10), 1);
        assert_eq!(Grain::Fixed(16).chunk_size(10), 16);
    }

    #[test]
    fn frame_is_most_urgent() {
        assert!(Priority::Frame < Priority::Normal);
        assert!(Priority::Normal < Priority::Background);
    }
}
