//! M2 criterion: a graph of ~50 tasks runs a frame without allocations (in the steady state).
//! M4: also with a `tracing` subscriber (a span per task) and with task labels for crash context.
//!
//! A separate test binary with a counting global allocator (DECISIONS.md #19).
// Test code: a panic is a test failure.
#![allow(clippy::unwrap_used, clippy::panic, clippy::arithmetic_side_effects)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use pragma_graph::{FrameGraph, FrameGraphBuilder, GraphError, Resources, TaskContext};
use pragma_jobs::{PoolConfig, ThreadPool, current_task};
use tracing::span::{Attributes, Id, Record};
use tracing::{self, Event, Metadata, Subscriber};

/// The system allocator, counting allocations while `COUNTING` is raised.
struct Counting;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn count() {
    if COUNTING.load(Ordering::Relaxed) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: all methods delegate to `System` with the same arguments and only add a counter,
// so the `GlobalAlloc` contract is upheld exactly as it is by `System`.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the `alloc` contract is passed through from the caller unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: see `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: `ptr` was allocated by this same allocator (i.e. `System`); the contract is passed through as is.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `System` through this allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

struct R<const N: usize>(Vec<u64>);

/// Tasks in which the current task label did not match the graph task.
static WRONG_LABEL: AtomicUsize = AtomicUsize::new(0);

fn task<const W: usize, const RD: usize>(g: &mut FrameGraphBuilder, name: String) {
    g.task(
        name,
        |t| t.writes::<R<W>>().reads::<R<RD>>(),
        |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
            // The same query a panic hook makes: no allocations.
            let labeled =
                current_task(|t| t.is_some_and(|t| t.kind() == "graph" && t.name() == ctx.name()));
            if !labeled {
                WRONG_LABEL.fetch_add(1, Ordering::Relaxed);
            }
            let src: u64 = ctx.read::<R<RD>>()?.0.iter().sum();
            for (i, x) in ctx.write::<R<W>>()?.0.iter_mut().enumerate() {
                *x = x.wrapping_add(src).wrapping_add(i as u64) % 1_000_003;
            }
            Ok(())
        },
    );
}

/// 50 tasks over 10 resources; conflicts are ordered by declaration (`ordered`).
fn graph() -> FrameGraph {
    let mut g = FrameGraph::builder().ordered();
    for i in 0..5 {
        task::<0, 1>(&mut g, format!("a{i}"));
        task::<1, 2>(&mut g, format!("b{i}"));
        task::<2, 3>(&mut g, format!("c{i}"));
        task::<3, 4>(&mut g, format!("d{i}"));
        task::<4, 0>(&mut g, format!("e{i}"));
        task::<5, 6>(&mut g, format!("f{i}"));
        task::<6, 7>(&mut g, format!("g{i}"));
        task::<7, 8>(&mut g, format!("h{i}"));
        task::<8, 9>(&mut g, format!("i{i}"));
        task::<9, 5>(&mut g, format!("j{i}"));
    }
    g.build().unwrap()
}

fn resources() -> Resources {
    let mut r = Resources::new();
    r.insert(R::<0>(vec![1; 64]));
    r.insert(R::<1>(vec![1; 64]));
    r.insert(R::<2>(vec![1; 64]));
    r.insert(R::<3>(vec![1; 64]));
    r.insert(R::<4>(vec![1; 64]));
    r.insert(R::<5>(vec![1; 64]));
    r.insert(R::<6>(vec![1; 64]));
    r.insert(R::<7>(vec![1; 64]));
    r.insert(R::<8>(vec![1; 64]));
    r.insert(R::<9>(vec![1; 64]));
    r
}

/// A `tracing` subscriber that counts spans and does not allocate itself: it checks that
/// frame instrumentation (a span per task) adds no allocations.
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

/// A single test function: the counter and the subscriber are global, and parallel tests in this
/// binary would spoil them. First without a subscriber, then with one.
#[test]
fn frame_run_does_not_allocate() {
    frames_without_allocations("no subscriber");
    assert_eq!(SPANS.load(Ordering::SeqCst), 0);
    tracing::subscriber::set_global_default(CountingSubscriber).unwrap();
    frames_without_allocations("tracing subscriber");
    assert!(
        SPANS.load(Ordering::SeqCst) >= 3 * 2000 * 50,
        "every graph task opens a span"
    );
    assert_eq!(WRONG_LABEL.load(Ordering::SeqCst), 0);
    panic_hook_reads_culprit_without_allocating();
}

static HOOK_ALLOCATIONS: AtomicUsize = AtomicUsize::new(usize::MAX);
static HOOK_SAW_CULPRIT: AtomicBool = AtomicBool::new(false);

/// The panic hook reads the culprit's label (a graph task), and the counting allocator sees zero
/// allocations during the read.
fn panic_hook_reads_culprit_without_allocating() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {
        let before = ALLOCATIONS.load(Ordering::SeqCst);
        COUNTING.store(true, Ordering::SeqCst);
        let saw = current_task(|t| t.is_some_and(|t| t.kind() == "graph" && t.name() == "culprit"));
        COUNTING.store(false, Ordering::SeqCst);
        HOOK_ALLOCATIONS.store(
            ALLOCATIONS.load(Ordering::SeqCst) - before,
            Ordering::SeqCst,
        );
        HOOK_SAW_CULPRIT.store(saw, Ordering::SeqCst);
    }));
    let pool = ThreadPool::new(PoolConfig::default().with_workers(2)).unwrap();
    let mut g = FrameGraph::builder();
    g.task(
        "culprit",
        |t| t,
        |_: &TaskContext<'_>| -> Result<(), GraphError> { panic!("boom") },
    );
    let mut graph = g.build().unwrap();
    assert!(graph.run(&pool, &mut Resources::new()).is_err());
    std::panic::set_hook(previous);
    assert!(HOOK_SAW_CULPRIT.load(Ordering::SeqCst));
    assert_eq!(HOOK_ALLOCATIONS.load(Ordering::SeqCst), 0);
}

fn frames_without_allocations(mode: &str) {
    for workers in [1, 2, 4] {
        let pool = ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap();
        let mut graph = graph();
        assert_eq!(graph.len(), 50);
        let mut res = resources();
        // Warm-up: per-thread caches (scope counters, crossbeam epochs) fill up in the first frames.
        for _ in 0..200 {
            graph.run(&pool, &mut res).unwrap();
        }
        ALLOCATIONS.store(0, Ordering::SeqCst);
        COUNTING.store(true, Ordering::SeqCst);
        for _ in 0..2000 {
            graph.run(&pool, &mut res).unwrap();
        }
        COUNTING.store(false, Ordering::SeqCst);
        let allocations = ALLOCATIONS.load(Ordering::SeqCst);
        assert_eq!(
            allocations, 0,
            "{mode}, {workers} workers: {allocations} allocations in 2000 frames"
        );
    }
}
