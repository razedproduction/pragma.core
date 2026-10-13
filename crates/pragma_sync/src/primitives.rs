//! Единая точка импорта атомиков и блокировок: под `--cfg loom` подменяются моделями `loom`,
//! чтобы модель-чекер видел каждый атомарный шаг протоколов этого крейта.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Condvar, Mutex};

#[cfg(not(loom))]
pub(crate) use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(not(loom))]
pub(crate) use std::sync::{Arc, Condvar, Mutex};
