//! Критерий M2: граф из ~50 задач исполняется за кадр без аллокаций (в установившемся режиме).
//! M4: и с подписчиком `tracing` (span на задачу), и с метками задач для крэш-контекста.
//!
//! Отдельный тестовый бинарник со считающим глобальным аллокатором (DECISIONS.md #19).
// Тестовый код: паника = провал теста.
#![allow(clippy::unwrap_used, clippy::panic, clippy::arithmetic_side_effects)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use pragma_base::tracing::span::{Attributes, Id, Record};
use pragma_base::tracing::{self, Event, Metadata, Subscriber};
use pragma_graph::{FrameGraph, FrameGraphBuilder, GraphError, Resources, TaskContext};
use pragma_jobs::{PoolConfig, ThreadPool, current_task};

/// Системный аллокатор, считающий аллокации, пока поднят `COUNTING`.
struct Counting;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn count() {
    if COUNTING.load(Ordering::Relaxed) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: все методы делегируют `System` с теми же аргументами и только добавляют счётчик,
// поэтому контракт `GlobalAlloc` выполняется ровно так же, как у `System`.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: контракт `alloc` передаётся вызывающим без изменений.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: см. `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: `ptr` выделен этим же аллокатором (то есть `System`), контракт передаётся как есть.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` выделен `System` через этот аллокатор.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

struct R<const N: usize>(Vec<u64>);

/// Задачи, в которых метка текущей задачи не совпала с задачей графа.
static WRONG_LABEL: AtomicUsize = AtomicUsize::new(0);

fn task<const W: usize, const RD: usize>(g: &mut FrameGraphBuilder, name: String) {
    g.task(
        name,
        |t| t.writes::<R<W>>().reads::<R<RD>>(),
        |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
            // Тот же запрос, что делает panic hook: без аллокаций.
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

/// 50 задач над 10 ресурсами; конфликты упорядочены по объявлению (`ordered`).
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

/// Подписчик `tracing`, который считает span и сам не аллоцирует: проверяет, что
/// инструментирование кадра (span на задачу) не добавляет аллокаций.
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

/// Одна функция-тест: счётчик и подписчик глобальные, параллельные тесты в этом бинарнике
/// их бы портили. Сначала без подписчика, затем с ним.
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

/// Panic hook читает метку виновника (задачи графа), а считающий аллокатор видит ноль
/// аллокаций на время чтения.
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
        // Прогрев: кэши потоков (счётчики областей, эпохи crossbeam) заполняются в первых кадрах.
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
