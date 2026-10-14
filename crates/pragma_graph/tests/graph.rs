//! РџРѕРІРµРґРµРЅРёРµ РіСЂР°С„Р° РєР°РґСЂР°: РїРѕСЂСЏРґРѕРє, РѕС€РёР±РєРё, РїР°РЅРёРєРё, РІР»РѕР¶РµРЅРЅС‹Р№ РїР°СЂР°Р»Р»РµР»РёР·Рј, СЌРєСЃРїРѕСЂС‚.
// РўРµСЃС‚РѕРІС‹Р№ РєРѕРґ: РїР°РЅРёРєР° = РїСЂРѕРІР°Р» С‚РµСЃС‚Р°, РїРµСЂРµРїРѕР»РЅРµРЅРёРµ РёСЃРєР»СЋС‡РµРЅРѕ СЂР°Р·РјРµСЂР°РјРё РІС…РѕРґР°.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pragma_graph::{FrameGraph, GraphError, Resources, TaskContext};
use pragma_jobs::{Grain, PoolConfig, ThreadPool};

fn pool(workers: usize) -> ThreadPool {
    ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap()
}

#[derive(Debug, PartialEq)]
struct Input(u32);
#[derive(Debug, PartialEq)]
struct Bodies(Vec<u64>);
#[derive(Debug, PartialEq)]
struct Transforms(Vec<u64>);
#[derive(Debug, PartialEq)]
struct Audio(u64);

type Log = Arc<Mutex<Vec<&'static str>>>;

fn logged(log: &Log, name: &'static str) -> impl Fn(&TaskContext<'_>) + Send + Sync + 'static {
    let log = Arc::clone(log);
    move |_| log.lock().unwrap().push(name)
}

fn position(log: &[&str], name: &str) -> usize {
    log.iter().position(|n| *n == name).unwrap()
}

/// РџСЂРёРјРµСЂ РёР· `docs/architecture.md` В§3.3.
fn frame() -> (FrameGraph, Resources) {
    let mut g = FrameGraph::builder();
    g.task(
        "input",
        |t| t.writes::<Input>(),
        |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
            ctx.write::<Input>()?.0 += 1;
            Ok(())
        },
    );
    g.task(
        "physics",
        |t| t.reads::<Input>().writes::<Bodies>().after("input"),
        |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
            let step = u64::from(ctx.read::<Input>()?.0);
            let mut bodies = ctx.write::<Bodies>()?;
            ctx.pool()
                .parallel_for(&mut bodies.0, Grain::Fixed(16), |b| *b += step)?;
            Ok(())
        },
    );
    g.task(
        "transforms",
        |t| t.reads::<Bodies>().writes::<Transforms>().after("physics"),
        |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
            let bodies = ctx.read::<Bodies>()?;
            let mut tr = ctx.write::<Transforms>()?;
            tr.0.clear();
            tr.0.extend(bodies.0.iter().map(|b| b * 2));
            Ok(())
        },
    );
    g.task(
        "audio",
        |t| {
            t.reads::<Transforms>()
                .writes::<Audio>()
                .after("transforms")
        },
        |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
            let sum = ctx.read::<Transforms>()?.0.iter().sum();
            ctx.write::<Audio>()?.0 = sum;
            Ok(())
        },
    );
    let mut r = Resources::new();
    r.insert(Input(0));
    r.insert(Bodies(vec![0; 100]));
    r.insert(Transforms(Vec::with_capacity(100)));
    r.insert(Audio(0));
    (g.build().unwrap(), r)
}

#[test]
fn architecture_example_runs_every_frame() {
    for workers in [1, 2, 4] {
        let pool = pool(workers);
        let (mut graph, mut res) = frame();
        assert_eq!(
            graph.order().collect::<Vec<_>>(),
            ["input", "physics", "transforms", "audio"]
        );
        for _ in 0..10 {
            graph.run(&pool, &mut res).unwrap();
        }
        // РљР°РґСЂ k РґРѕР±Р°РІР»СЏРµС‚ k Рє РєР°Р¶РґРѕРјСѓ С‚РµР»Сѓ: 1 + 2 + вЂ¦ + 10 = 55.
        assert_eq!(res.get::<Bodies>().unwrap().0, vec![55; 100]);
        assert_eq!(res.get::<Audio>().unwrap().0, 55 * 2 * 100);
    }
}

#[test]
fn independent_tasks_all_run_and_respect_edges() {
    let log: Log = Arc::default();
    let mut g = FrameGraph::builder();
    g.task("a", |t| t, logged(&log, "a"));
    g.task("b", |t| t.after("a"), logged(&log, "b"));
    g.task("c", |t| t.after("a"), logged(&log, "c"));
    g.task("d", |t| t.after("b").after("c"), logged(&log, "d"));
    g.task("lonely", |t| t, logged(&log, "lonely"));
    let mut graph = g.build().unwrap();
    assert_eq!(graph.len(), 5);
    let pool = pool(3);
    let mut res = Resources::new();
    for _ in 0..50 {
        log.lock().unwrap().clear();
        graph.run(&pool, &mut res).unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 5, "each task runs exactly once: {log:?}");
        assert!(position(&log, "a") < position(&log, "b"));
        assert!(position(&log, "a") < position(&log, "c"));
        assert!(position(&log, "b") < position(&log, "d"));
        assert!(position(&log, "c") < position(&log, "d"));
    }
}

#[test]
fn readers_of_one_resource_may_run_together() {
    let mut g = FrameGraph::builder();
    for name in ["r1", "r2", "r3"] {
        g.task(
            name,
            |t| t.reads::<Bodies>(),
            |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
                let _a = ctx.read::<Bodies>()?;
                let _b = ctx.read::<Bodies>()?;
                Ok(())
            },
        );
    }
    let mut graph = g.build().unwrap();
    assert_eq!(graph.edges().count(), 0);
    let mut res = Resources::new();
    res.insert(Bodies(vec![]));
    graph.run(&pool(2), &mut res).unwrap();
}

#[test]
fn ordered_mode_uses_declaration_order() {
    let log: Log = Arc::default();
    let mut g = FrameGraph::builder().ordered();
    g.task("first", |t| t.writes::<Input>(), logged(&log, "first"));
    g.task("second", |t| t.reads::<Input>(), logged(&log, "second"));
    g.task("third", |t| t.writes::<Input>(), logged(&log, "third"));
    let mut graph = g.build().unwrap();
    assert_eq!(
        graph.edges().collect::<Vec<_>>(),
        [("first", "second"), ("second", "third")]
    );
    let mut res = Resources::new();
    res.insert(Input(0));
    graph.run(&pool(2), &mut res).unwrap();
    assert_eq!(*log.lock().unwrap(), ["first", "second", "third"]);
}

#[test]
fn build_errors() {
    let mut g = FrameGraph::builder();
    g.task("a", |t| t.writes::<Input>(), |_| ());
    g.task("b", |t| t.writes::<Input>(), |_| ());
    let err = g.build().unwrap_err();
    assert!(
        matches!(&err, GraphError::AmbiguousOrder { a, b, resource, .. }
            if a == "a" && b == "b" && resource == std::any::type_name::<Input>()),
        "{err:?}"
    );

    let mut g = FrameGraph::builder();
    g.task("a", |t| t.after("b"), |_| ());
    g.task("b", |t| t.after("a"), |_| ());
    let err = g.build().unwrap_err();
    assert!(
        matches!(&err, GraphError::Cycle { tasks, .. } if tasks == &["a", "b"]),
        "{err:?}"
    );

    let mut g = FrameGraph::builder();
    g.task("a", |t| t.before("ghost"), |_| ());
    let err = g.build().unwrap_err();
    assert!(
        matches!(&err, GraphError::UnknownDependency { task, missing, .. }
            if task == "a" && missing == "ghost"),
        "{err:?}"
    );

    let mut g = FrameGraph::builder();
    g.task("a", |t| t, |_| ());
    g.task("a", |t| t, |_| ());
    let err = g.build().unwrap_err();
    assert!(
        matches!(&err, GraphError::DuplicateTask { task, .. } if task == "a"),
        "{err:?}"
    );
}

#[test]
fn missing_resource_is_reported_before_the_frame() {
    let ran = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ran);
    let mut g = FrameGraph::builder();
    g.task(
        "free",
        |t| t,
        move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        },
    );
    g.task("needs", |t| t.reads::<Audio>(), |_| ());
    let mut graph = g.build().unwrap();
    let err = graph.run(&pool(1), &mut Resources::new()).unwrap_err();
    assert!(
        matches!(&err, GraphError::MissingResource { task, resource, .. }
            if task == "needs" && resource == std::any::type_name::<Audio>()),
        "{err:?}"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0, "no task runs");
}

#[test]
fn undeclared_and_double_borrows_are_errors() {
    let mut g = FrameGraph::builder();
    g.task(
        "sneaky",
        |t| t.reads::<Input>(),
        |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
            ctx.write::<Input>()?.0 = 7;
            Ok(())
        },
    );
    g.task(
        "greedy",
        |t| t.writes::<Audio>(),
        |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
            let _first = ctx.write::<Audio>()?;
            let _second = ctx.write::<Audio>()?;
            Ok(())
        },
    );
    let mut graph = g.build().unwrap();
    let mut res = Resources::new();
    res.insert(Input(0));
    res.insert(Audio(0));
    // РћС€РёР±РєР° Р·Р°РґР°С‡Рё СЃ РЅР°РёРјРµРЅСЊС€РёРј РёРЅРґРµРєСЃРѕРј СЂРµРіРёСЃС‚СЂР°С†РёРё.
    let err = graph.run(&pool(2), &mut res).unwrap_err();
    assert!(
        matches!(&err, GraphError::UndeclaredAccess { task, resource, access: "write", .. }
            if task == "sneaky" && resource == std::any::type_name::<Input>()),
        "{err:?}"
    );
    assert_eq!(res.get::<Input>().unwrap().0, 0);

    let mut g = FrameGraph::builder();
    g.task(
        "greedy",
        |t| t.writes::<Audio>(),
        |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
            let _first = ctx.write::<Audio>()?;
            let _second = ctx.write::<Audio>()?;
            Ok(())
        },
    );
    let err = g.build().unwrap().run(&pool(1), &mut res).unwrap_err();
    assert!(matches!(err, GraphError::BorrowConflict { ref task, .. } if task == "greedy"));
}

#[test]
fn failure_skips_dependents_but_not_independent_tasks() {
    let log: Log = Arc::default();
    let mut g = FrameGraph::builder();
    g.task("ok", |t| t, logged(&log, "ok"));
    g.task(
        "boom",
        |t| t.writes::<Input>(),
        |_| -> Result<(), String> { Err("disk on fire".into()) },
    );
    g.task(
        "after_boom",
        |t| t.after("boom"),
        logged(&log, "after_boom"),
    );
    g.task(
        "panics",
        |t| t,
        |_| -> Result<(), String> { panic!("kaboom") },
    );
    g.task(
        "after_panic",
        |t| t.after("panics"),
        logged(&log, "after_panic"),
    );
    g.task(
        "independent",
        |t| t.after("ok"),
        logged(&log, "independent"),
    );
    let mut graph = g.build().unwrap();
    let mut res = Resources::new();
    res.insert(Input(0));
    for workers in [1, 3] {
        let pool = pool(workers);
        for _ in 0..20 {
            log.lock().unwrap().clear();
            let err = graph.run(&pool, &mut res).unwrap_err();
            // `boom` Р·Р°СЂРµРіРёСЃС‚СЂРёСЂРѕРІР°РЅР° СЂР°РЅСЊС€Рµ `panics`: РІС‹Р±РѕСЂ РѕС€РёР±РєРё РЅРµ Р·Р°РІРёСЃРёС‚ РѕС‚ СЂР°СЃРїРёСЃР°РЅРёСЏ.
            assert!(
                matches!(&err, GraphError::TaskFailed { task, message, .. }
                    if task == "boom" && message == "disk on fire"),
                "{err:?}"
            );
            let mut log = log.lock().unwrap().clone();
            log.sort_unstable();
            assert_eq!(log, ["independent", "ok"]);
        }
    }
}

#[test]
fn panic_is_reported_with_task_name_and_pool_survives() {
    let mut g = FrameGraph::builder();
    g.task(
        "panics",
        |t| t.writes::<Input>(),
        |ctx: &TaskContext<'_>| -> Result<(), String> {
            let _guard = ctx.write::<Input>();
            panic!("bad frame");
        },
    );
    let mut graph = g.build().unwrap();
    let pool = pool(2);
    let mut res = Resources::new();
    res.insert(Input(0));
    let err = graph.run(&pool, &mut res).unwrap_err();
    assert!(
        matches!(&err, GraphError::TaskPanicked { task, message, .. }
            if task == "panics" && message == "bad frame"),
        "{err:?}"
    );
    // РћС‚СЂР°РІР»РµРЅРЅС‹Р№ РїР°РЅРёРєРѕР№ СЃР»РѕС‚ РѕСЃС‚Р°С‘С‚СЃСЏ РґРѕСЃС‚СѓРїРЅС‹Рј, РїСѓР» Р¶РёРІ.
    assert_eq!(res.get::<Input>().unwrap().0, 0);
    assert_eq!(pool.join(|| 1, || 2).unwrap(), (1, 2));
}

#[test]
fn task_errors_convert() {
    #[derive(Debug)]
    struct MyError;
    impl std::fmt::Display for MyError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("my error")
        }
    }
    impl std::error::Error for MyError {}

    let mut g = FrameGraph::builder();
    g.task("custom", |t| t, |_| -> Result<(), MyError> { Err(MyError) });
    let err = g.build().unwrap().run(&pool(1), &mut Resources::new());
    assert!(
        matches!(&err, Err(GraphError::TaskFailed { task, message, .. })
            if task == "custom" && message == "my error"),
        "{err:?}"
    );
}

#[test]
fn empty_graph_runs() {
    let mut graph = FrameGraph::builder().build().unwrap();
    assert!(graph.is_empty());
    graph.run(&pool(1), &mut Resources::new()).unwrap();
}

#[test]
fn deep_chain_does_not_grow_the_stack() {
    let mut g = FrameGraph::builder();
    let count = Arc::new(AtomicUsize::new(0));
    for i in 0..5000 {
        let count = Arc::clone(&count);
        g.task(
            format!("t{i}"),
            |t| {
                if i == 0 {
                    t
                } else {
                    t.after(format!("t{}", i - 1))
                }
            },
            move |_| {
                count.fetch_add(1, Ordering::Relaxed);
            },
        );
    }
    let mut graph = g.build().unwrap();
    graph.run(&pool(2), &mut Resources::new()).unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 5000);
}

#[test]
fn wide_graph_on_one_worker() {
    let mut g = FrameGraph::builder();
    let count = Arc::new(AtomicUsize::new(0));
    g.task("root", |t| t, |_| ());
    for i in 0..1000 {
        let count = Arc::clone(&count);
        g.task(
            format!("leaf{i}"),
            |t| t.after("root"),
            move |ctx: &TaskContext<'_>| {
                // Р’Р»РѕР¶РµРЅРЅС‹Р№ РїР°СЂР°Р»Р»РµР»РёР·Рј РёР· Р·Р°РґР°С‡Рё РіСЂР°С„Р° РЅР° РѕРґРЅРѕРј РІРѕСЂРєРµСЂРµ РЅРµ РґРµРґР»РѕС‡РёС‚.
                let (a, b) = ctx.pool().join(|| 1, || 2).unwrap();
                count.fetch_add(a + b, Ordering::Relaxed);
            },
        );
    }
    let mut graph = g.build().unwrap();
    graph.run(&pool(1), &mut Resources::new()).unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 3000);
}

#[test]
fn same_graph_over_two_resource_sets() {
    let (mut graph, mut main) = frame();
    let (_, mut extract) = frame();
    let pool = pool(2);
    graph.run(&pool, &mut main).unwrap();
    graph.run(&pool, &mut main).unwrap();
    graph.run(&pool, &mut extract).unwrap();
    assert_eq!(main.get::<Input>().unwrap().0, 2);
    assert_eq!(extract.get::<Input>().unwrap().0, 1);
}

#[test]
fn dot_and_mermaid_export() {
    let (graph, _) = frame();
    let dot = graph.to_dot();
    assert!(dot.starts_with("digraph frame {"));
    assert!(
        dot.contains(r#"t1 [label="physics\nR: Input\nW: Bodies"];"#),
        "{dot}"
    );
    assert!(dot.contains("t0 -> t1;"));
    assert!(dot.contains("t2 -> t3;"));
    assert!(dot.trim_end().ends_with('}'));

    let mermaid = graph.to_mermaid();
    assert!(mermaid.starts_with("flowchart LR\n"));
    assert!(mermaid.contains(r#"t0["input<br/>W: Input"]"#), "{mermaid}");
    assert!(mermaid.contains("t1 --> t2"));

    let mut g = FrameGraph::builder();
    g.task(r#"say "hi""#, |t| t, |_| ());
    let graph = g.build().unwrap();
    assert!(graph.to_dot().contains(r#"label="say \"hi\"""#));
    assert!(graph.to_mermaid().contains("say #quot;hi#quot;"));
}
