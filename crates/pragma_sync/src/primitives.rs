//! The single import point for atomics and locks: under `--cfg loom` they are replaced by `loom`
//! models, so that the model checker sees every atomic step of this crate's protocols.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Condvar, Mutex};

#[cfg(not(loom))]
pub(crate) use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(not(loom))]
pub(crate) use std::sync::{Arc, Condvar, Mutex};
