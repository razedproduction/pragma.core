use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

/// A resource key: the type and its name for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResourceKey {
    pub(crate) id: TypeId,
    pub(crate) name: &'static str,
}

impl ResourceKey {
    pub(crate) fn of<T: 'static>() -> Self {
        Self {
            id: TypeId::of::<T>(),
            name: type_name::<T>(),
        }
    }
}

/// A resource slot: a type-erased `RwLock<T>`.
struct Slot {
    name: &'static str,
    cell: Box<dyn Any + Send + Sync>,
}

impl Slot {
    fn typed<T: 'static>(&self) -> Option<&RwLock<T>> {
        self.cell.downcast_ref::<RwLock<T>>()
    }
}

/// Why a resource could not be borrowed.
pub(crate) enum Borrow {
    Missing,
    Busy,
}

/// The frame's resource storage: one value per type.
///
/// Each resource lives in its own typed slot with a read/write lock. The graph
/// guarantees at build time that concurrently running tasks do not conflict in their access,
/// so locks are never waited on during a frame: a busy slot means an error ([`GraphError::BorrowConflict`]),
/// not a wait. The graph does not own the resources: the same graph can run over different
/// `Resources` instances (for example over an "extract" copy of the state for rendering).
///
/// [`GraphError::BorrowConflict`]: crate::GraphError::BorrowConflict
#[derive(Default)]
pub struct Resources {
    slots: HashMap<TypeId, Slot>,
}

impl fmt::Debug for Resources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Sorted: `HashMap` iteration order is random, and the output must be reproducible.
        let mut names: Vec<&str> = self.slots.values().map(|s| s.name).collect();
        names.sort_unstable();
        f.debug_struct("Resources").field("types", &names).finish()
    }
}

impl Resources {
    /// An empty storage.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores a resource; returns the previous value of this type, if there was one.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        let slot = Slot {
            name: type_name::<T>(),
            cell: Box::new(RwLock::new(value)),
        };
        let old = self.slots.insert(TypeId::of::<T>(), slot)?;
        Self::into_value(old)
    }

    /// Removes a resource and returns it.
    pub fn remove<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        let old = self.slots.remove(&TypeId::of::<T>())?;
        Self::into_value(old)
    }

    fn into_value<T: 'static>(slot: Slot) -> Option<T> {
        let cell = slot.cell.downcast::<RwLock<T>>().ok()?;
        Some(cell.into_inner().unwrap_or_else(PoisonError::into_inner))
    }

    /// Whether there is a resource of type `T`.
    #[must_use]
    pub fn contains<T: 'static>(&self) -> bool {
        self.slots.contains_key(&TypeId::of::<T>())
    }

    pub(crate) fn contains_key(&self, key: ResourceKey) -> bool {
        self.slots.contains_key(&key.id)
    }

    /// Number of resources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the storage is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// A resource for reading. Outside a frame (the graph runs only over `&mut Resources`) a slot
    /// is never write-locked, so `None` only means "no such resource".
    #[must_use]
    pub fn get<T: 'static>(&self) -> Option<Res<'_, T>> {
        self.read::<T>().ok()
    }

    /// A resource for writing: no locking, via an exclusive reference.
    pub fn get_mut<T: 'static>(&mut self) -> Option<&mut T> {
        let slot = self.slots.get_mut(&TypeId::of::<T>())?;
        let cell = slot.cell.downcast_mut::<RwLock<T>>()?;
        Some(cell.get_mut().unwrap_or_else(PoisonError::into_inner))
    }

    fn slot<T: 'static>(&self) -> Result<&RwLock<T>, Borrow> {
        self.slots
            .get(&TypeId::of::<T>())
            .and_then(Slot::typed::<T>)
            .ok_or(Borrow::Missing)
    }

    // Poisoning (a task panicking mid-write) is no reason to panic: that task's error has already
    // been returned from the frame, and whether the data is consistent is for the user to decide.
    pub(crate) fn read<T: 'static>(&self) -> Result<Res<'_, T>, Borrow> {
        match self.slot::<T>()?.try_read() {
            Ok(guard) => Ok(Res(guard)),
            Err(TryLockError::Poisoned(poisoned)) => Ok(Res(poisoned.into_inner())),
            Err(TryLockError::WouldBlock) => Err(Borrow::Busy),
        }
    }

    pub(crate) fn write<T: 'static>(&self) -> Result<ResMut<'_, T>, Borrow> {
        match self.slot::<T>()?.try_write() {
            Ok(guard) => Ok(ResMut(guard)),
            Err(TryLockError::Poisoned(poisoned)) => Ok(ResMut(poisoned.into_inner())),
            Err(TryLockError::WouldBlock) => Err(Borrow::Busy),
        }
    }
}

/// A resource borrowed for reading.
pub struct Res<'a, T>(RwLockReadGuard<'a, T>);

impl<T> Deref for Res<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: fmt::Debug> fmt::Debug for Res<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// A resource borrowed for writing.
pub struct ResMut<'a, T>(RwLockWriteGuard<'a, T>);

impl<T> Deref for ResMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for ResMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: fmt::Debug> fmt::Debug for ResMut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_replace_remove() {
        let mut r = Resources::new();
        assert!(r.is_empty());
        assert_eq!(r.insert(1_u32), None);
        assert_eq!(r.insert(2_u32), Some(1));
        assert!(r.contains::<u32>());
        assert!(!r.contains::<u64>());
        *r.get_mut::<u32>().unwrap() += 1;
        assert_eq!(*r.get::<u32>().unwrap(), 3);
        assert_eq!(r.remove::<u32>(), Some(3));
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn write_excludes_other_borrows() {
        let mut r = Resources::new();
        r.insert(String::from("x"));
        let w = r.write::<String>().ok().unwrap();
        assert!(matches!(r.read::<String>(), Err(Borrow::Busy)));
        assert!(matches!(r.write::<String>(), Err(Borrow::Busy)));
        drop(w);
        let a = r.read::<String>().ok().unwrap();
        let b = r.read::<String>().ok().unwrap();
        assert_eq!(*a, *b);
        assert!(matches!(r.read::<u8>(), Err(Borrow::Missing)));
    }

    #[test]
    fn debug_lists_types_sorted() {
        let mut r = Resources::new();
        r.insert(1_u64);
        r.insert(1_u8);
        assert_eq!(format!("{r:?}"), r#"Resources { types: ["u64", "u8"] }"#);
    }
}
