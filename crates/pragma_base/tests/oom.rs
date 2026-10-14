//! M5: рост `HandlePool` при нехватке памяти — `BaseError::OutOfMemory`, а не `abort`;
//! `remove` и вставка в освобождённый слот не выделяют память вовсе.
//!
//! Отдельный тестовый бинарник с отказывающим глобальным аллокатором (DECISIONS.md #37).
// Тестовый код: паника = провал теста.
#![allow(clippy::unwrap_used, clippy::panic, clippy::arithmetic_side_effects)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pragma_base::{BaseError, HandlePool};

/// Системный аллокатор, который на потоке с поднятым `FAIL` отказывает в любом выделении.
struct Failing;

thread_local! {
    // Константная инициализация: доступ из аллокатора сам ничего не выделяет.
    static FAIL: Cell<bool> = const { Cell::new(false) };
}

fn failing() -> bool {
    FAIL.try_with(Cell::get).unwrap_or(false)
}

// SAFETY: методы делегируют `System` с теми же аргументами; отказ — возврат null, что контракт
// `GlobalAlloc::alloc`/`realloc` прямо разрешает (вызывающий обязан обработать null).
unsafe impl GlobalAlloc for Failing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if failing() {
            return std::ptr::null_mut();
        }
        // SAFETY: контракт `alloc` передаётся вызывающим без изменений.
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if failing() {
            return std::ptr::null_mut();
        }
        // SAFETY: `ptr` выделен этим же аллокатором (то есть `System`), контракт передаётся как есть.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` выделен `System` через этот аллокатор.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Failing = Failing;

/// Выполняет `f`, пока аллокатор отказывает на этом потоке.
fn without_memory<R>(f: impl FnOnce() -> R) -> R {
    FAIL.with(|fail| fail.set(true));
    let result = f();
    FAIL.with(|fail| fail.set(false));
    result
}

#[test]
fn growth_without_memory_is_an_error_and_pool_stays_usable() {
    let mut pool: HandlePool<u64> = HandlePool::new();
    assert!(matches!(
        without_memory(|| pool.insert(1)),
        Err(BaseError::OutOfMemory { slots: 0, .. })
    ));
    assert!(pool.is_empty());

    // С памятью пул растёт как обычно.
    let handles: Vec<_> = (0..100).map(|v| pool.insert(v).unwrap()).collect();

    // Освобождение и повторное использование слота не выделяют память.
    let reused = without_memory(|| {
        let value = pool.remove(handles[10]).unwrap();
        let reused = pool.insert(value + 1000).unwrap();
        (value, reused)
    });
    assert_eq!(reused.0, 10);
    assert_eq!(pool.get(reused.1), Some(&1010));

    // Рано или поздно рост требует выделения — ошибка с текущим размером, без `abort`.
    let error = without_memory(|| (0..1_000_000_u64).find_map(|v| pool.insert(v).err()));
    let Some(BaseError::OutOfMemory { slots, .. }) = error else {
        panic!("expected OutOfMemory, got {error:?}");
    };
    assert_eq!(slots, pool.len());

    // После отказа пул цел: все живые хэндлы на месте, рост снова работает.
    assert_eq!(pool.get(handles[0]), Some(&0));
    assert!(pool.insert(7).is_ok());
}
