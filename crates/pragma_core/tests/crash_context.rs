//! Контекст задачи для крэш-репортов (M4): panic hook называет виновника — задачу графа,
//! `spawn`, async-задачу или blocking-замыкание.
//!
//! Один тест на бинарник: panic hook глобальный.
// Тестовый код: паника = провал теста.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::panic;
use std::sync::{Mutex, PoisonError};

use pragma_core::prelude::*;

/// Что увидел hook: метка задачи (цепочка) и сообщение паники.
static SEEN: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

fn seen_for(message: &str) -> Option<String> {
    SEEN.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|(_, m)| m.contains(message))
        .map(|(label, _)| label.clone())
}

#[test]
fn panic_hook_names_the_culprit() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(|info| {
        let label = current_task(|task| task.map(|t| t.to_string()).unwrap_or_default());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        SEEN.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((label, message));
    }));

    let pool = ThreadPool::new(PoolConfig::default().with_workers(2)).unwrap();

    // Задача графа.
    let mut g = FrameGraph::builder();
    g.task(
        "physics",
        |t| t,
        |_: &TaskContext<'_>| -> Result<(), GraphError> { panic!("graph task exploded") },
    );
    let mut graph = g.build().unwrap();
    assert!(graph.run(&pool, &mut Resources::new()).is_err());

    // `spawn` с именем.
    let spawned = pool.spawn_named("bake-navmesh", Priority::Background, || -> u32 {
        panic!("spawned task exploded")
    });
    assert!(spawned.join().is_err());

    // Async-задача и blocking-замыкание.
    let exec = Executor::new(&pool, AsyncConfig::default()).unwrap();
    let failed = exec
        .spawn_async_named("load-level", async { panic!("async task exploded") })
        .wait();
    assert!(matches!(failed, Err(AsyncError::Panicked { .. })));
    let blocking = exec
        .unblock(|| -> u32 { panic!("blocking read exploded") })
        .wait();
    assert!(matches!(blocking, Err(AsyncError::Panicked { .. })));

    panic::set_hook(previous);

    assert_eq!(
        seen_for("graph task exploded").as_deref(),
        Some("graph `physics`")
    );
    assert_eq!(
        seen_for("spawned task exploded").as_deref(),
        Some("spawn `bake-navmesh`")
    );
    let async_label = seen_for("async task exploded").unwrap();
    assert!(
        async_label.starts_with("async `load-level`"),
        "{async_label}"
    );
    assert_eq!(
        seen_for("blocking read exploded").as_deref(),
        Some("unblock `unblock`")
    );
}
