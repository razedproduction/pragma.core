use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use crate::BaseError;

/// Generational-хэндл: (индекс, поколение).
///
/// После удаления значения поколение слота растёт, поэтому старый хэндл
/// никогда не адресует новое значение в том же слоте.
pub struct Handle<T> {
    index: u32,
    generation: u32,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
    /// Индекс слота.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// Поколение слота на момент выдачи хэндла.
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

// Ручные impl-ы: derive потребовал бы `T: Clone/Eq/...`, хотя `T` в хэндле не хранится.
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

/// Пул значений, адресуемых [`Handle`]. Освобождённые слоты переиспользуются.
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

impl<T> HandlePool<T> {
    /// Пустой пул.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            len: 0,
        }
    }

    /// Число живых значений.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Пул пуст.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Кладёт значение и возвращает хэндл на него.
    pub fn insert(&mut self, value: T) -> Result<Handle<T>, BaseError> {
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                let index =
                    u32::try_from(self.slots.len()).map_err(|_| BaseError::CapacityExhausted)?;
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

    /// Значение по хэндлу или `None`, если хэндл устарел.
    #[must_use]
    pub fn get(&self, handle: Handle<T>) -> Option<&T> {
        self.slots
            .get(usize::try_from(handle.index).ok()?)
            .filter(|s| s.generation == handle.generation)
            .and_then(|s| s.value.as_ref())
    }

    /// Изменяемая ссылка по хэндлу или `None`, если хэндл устарел.
    #[must_use]
    pub fn get_mut(&mut self, handle: Handle<T>) -> Option<&mut T> {
        self.slot_mut(handle.index)
            .filter(|s| s.generation == handle.generation)
            .and_then(|s| s.value.as_mut())
    }

    /// Удаляет значение; хэндл и все его копии становятся устаревшими.
    pub fn remove(&mut self, handle: Handle<T>) -> Result<T, BaseError> {
        let slot = self
            .slot_mut(handle.index)
            .filter(|s| s.generation == handle.generation)
            .ok_or_else(|| handle.stale())?;
        let value = slot.value.take().ok_or_else(|| handle.stale())?;
        // Слот с исчерпанным поколением выводится из оборота навсегда,
        // чтобы поколение никогда не повторилось.
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
        /// Инвариант: живые хэндлы видят свои значения, удалённые — никогда.
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
