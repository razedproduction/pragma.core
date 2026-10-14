use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use crate::BaseError;

/// A generational handle: (index, generation).
///
/// Removing a value bumps the slot's generation, so an old handle
/// never addresses a new value in the same slot.
pub struct Handle<T> {
    index: u32,
    generation: u32,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
    /// Slot index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// Slot generation at the time the handle was issued.
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation
    }

    fn stale(self) -> BaseError {
        BaseError::StaleHandle {
            index: self.index,
            generation: self.generation,
        }
    }
}

// Manual impls: derive would require `T: Clone/Eq/...` even though no `T` is stored in the handle.
impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Handle<T> {}
impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.generation == other.generation
    }
}
impl<T> Eq for Handle<T> {}
impl<T> Hash for Handle<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state);
        self.generation.hash(state);
    }
}
impl<T> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Handle({}v{})", self.index, self.generation)
    }
}

struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

/// A pool of values addressed by [`Handle`]. Freed slots are reused.
pub struct HandlePool<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    len: usize,
}

impl<T> Default for HandlePool<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for HandlePool<T> {
    /// Live values with their handles.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let live = self.slots.iter().zip(0_u32..).filter_map(|(slot, index)| {
            let handle = Handle::<T> {
                index,
                generation: slot.generation,
                _marker: PhantomData,
            };
            slot.value.as_ref().map(|value| (handle, value))
        });
        f.debug_map().entries(live).finish()
    }
}

impl<T> HandlePool<T> {
    /// An empty pool.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            len: 0,
        }
    }

    /// Number of live values.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the pool is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Stores a value and returns a handle to it.
    ///
    /// The pool grows via `try_reserve`: running out of memory is [`BaseError::OutOfMemory`], not
    /// `abort`. Room in the free list is reserved together with the slot, so
    /// [`remove`](Self::remove) never allocates.
    pub fn insert(&mut self, value: T) -> Result<Handle<T>, BaseError> {
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                let slots = self.slots.len();
                let index = u32::try_from(slots).map_err(|_| BaseError::CapacityExhausted)?;
                let oom = |_| BaseError::OutOfMemory { slots };
                self.slots.try_reserve(1).map_err(oom)?;
                // `free.len() ≤ slots`, so the subtraction cannot overflow.
                let free_room = slots.saturating_add(1).saturating_sub(self.free.len());
                self.free.try_reserve(free_room).map_err(oom)?;
                self.slots.push(Slot {
                    generation: 0,
                    value: None,
                });
                index
            }
        };
        let slot = self.slot_mut(index).ok_or(BaseError::CapacityExhausted)?;
        slot.value = Some(value);
        let generation = slot.generation;
        self.len = self.len.saturating_add(1);
        Ok(Handle {
            index,
            generation,
            _marker: PhantomData,
        })
    }

    /// The value behind the handle, or `None` if the handle is stale.
    #[must_use]
    pub fn get(&self, handle: Handle<T>) -> Option<&T> {
        self.slots
            .get(usize::try_from(handle.index).ok()?)
            .filter(|s| s.generation == handle.generation)
            .and_then(|s| s.value.as_ref())
    }

    /// A mutable reference behind the handle, or `None` if the handle is stale.
    #[must_use]
    pub fn get_mut(&mut self, handle: Handle<T>) -> Option<&mut T> {
        self.slot_mut(handle.index)
            .filter(|s| s.generation == handle.generation)
            .and_then(|s| s.value.as_mut())
    }

    /// Removes the value; the handle and all its copies become stale.
    pub fn remove(&mut self, handle: Handle<T>) -> Result<T, BaseError> {
        let slot = self
            .slot_mut(handle.index)
            .filter(|s| s.generation == handle.generation)
            .ok_or_else(|| handle.stale())?;
        let value = slot.value.take().ok_or_else(|| handle.stale())?;
        // A slot whose generation is exhausted is retired forever,
        // so that a generation never repeats.
        match slot.generation.checked_add(1) {
            Some(next) => {
                slot.generation = next;
                self.free.push(handle.index);
            }
            None => slot.generation = u32::MAX,
        }
        self.len = self.len.saturating_sub(1);
        Ok(value)
    }

    fn slot_mut(&mut self, index: u32) -> Option<&mut Slot<T>> {
        self.slots.get_mut(usize::try_from(index).ok()?)
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn slot_reuse_bumps_generation() {
        let mut pool = HandlePool::new();
        let a = pool.insert(1).unwrap();
        pool.remove(a).unwrap();
        let b = pool.insert(2).unwrap();
        assert_eq!(a.index(), b.index());
        assert_ne!(a.generation(), b.generation());
        assert_eq!(pool.get(a), None);
        assert_eq!(pool.get(b), Some(&2));
        assert!(pool.remove(a).is_err());
    }

    proptest! {
        /// Invariant: live handles see their values, removed ones never do.
        #[test]
        fn handles_never_alias(ops in proptest::collection::vec(any::<(bool, u8)>(), 0..256)) {
            let mut pool = HandlePool::new();
            let mut live: Vec<(Handle<u32>, u32)> = Vec::new();
            let mut dead: Vec<Handle<u32>> = Vec::new();
            for (n, (insert, pick)) in ops.into_iter().enumerate() {
                if insert || live.is_empty() {
                    let v = u32::try_from(n).unwrap();
                    live.push((pool.insert(v).unwrap(), v));
                } else {
                    let (h, v) = live.swap_remove(usize::from(pick) % live.len());
                    prop_assert_eq!(pool.remove(h).unwrap(), v);
                    dead.push(h);
                }
                prop_assert_eq!(pool.len(), live.len());
            }
            for (h, v) in &live {
                prop_assert_eq!(pool.get(*h), Some(v));
            }
            for h in &dead {
                prop_assert_eq!(pool.get(*h), None);
            }
        }
    }
}
