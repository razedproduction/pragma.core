use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

/// Ключ ресурса: тип и его имя для сообщений об ошибках.
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

/// Слот ресурса: `RwLock<T>` со стёртым типом.
struct Slot {
    name: &'static str,
    cell: Box<dyn Any + Send + Sync>,
}

impl Slot {
    fn typed<T: 'static>(&self) -> Option<&RwLock<T>> {
        self.cell.downcast_ref::<RwLock<T>>()
    }
}

/// Почему не удалось взять ресурс.
pub(crate) enum Borrow {
    Missing,
    Busy,
}

/// Хранилище ресурсов кадра: по одному значению каждого типа.
///
/// Каждый ресурс лежит в своём типизированном слоте с блокировкой чтения/записи. Граф
/// гарантирует при сборке, что одновременно исполняемые задачи не конфликтуют по доступу,
/// поэтому в кадре блокировки не ждут: занятый слот означает ошибку ([`GraphError::BorrowConflict`]),
/// а не ожидание. Граф не владеет ресурсами: один и тот же граф можно исполнять над разными
/// экземплярами `Resources` (например, над «extract»-копией состояния для рендера).
///
/// [`GraphError::BorrowConflict`]: crate::GraphError::BorrowConflict
#[derive(Default)]
pub struct Resources {
    slots: HashMap<TypeId, Slot>,
}

impl fmt::Debug for Resources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Сортировка: порядок итерации `HashMap` случаен, а вывод должен быть воспроизводимым.
        let mut names: Vec<&str> = self.slots.values().map(|s| s.name).collect();
        names.sort_unstable();
        f.debug_struct("Resources").field("types", &names).finish()
    }
}

impl Resources {
    /// Пустое хранилище.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Кладёт ресурс; возвращает прежнее значение этого типа, если оно было.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        let slot = Slot {
            name: type_name::<T>(),
            cell: Box::new(RwLock::new(value)),
        };
        let old = self.slots.insert(TypeId::of::<T>(), slot)?;
        Self::into_value(old)
    }

    /// Убирает ресурс и возвращает его.
    pub fn remove<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        let old = self.slots.remove(&TypeId::of::<T>())?;
        Self::into_value(old)
    }

    fn into_value<T: 'static>(slot: Slot) -> Option<T> {
        let cell = slot.cell.downcast::<RwLock<T>>().ok()?;
        Some(cell.into_inner().unwrap_or_else(PoisonError::into_inner))
    }

    /// Есть ли ресурс типа `T`.
    #[must_use]
    pub fn contains<T: 'static>(&self) -> bool {
        self.slots.contains_key(&TypeId::of::<T>())
    }

    pub(crate) fn contains_key(&self, key: ResourceKey) -> bool {
        self.slots.contains_key(&key.id)
    }

    /// Число ресурсов.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Хранилище пусто.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Ресурс на чтение. Вне кадра (граф исполняется только по `&mut Resources`) слот
    /// не бывает занят на запись, поэтому `None` значит только «ресурса нет».
    #[must_use]
    pub fn get<T: 'static>(&self) -> Option<Res<'_, T>> {
        self.read::<T>().ok()
    }

    /// Ресурс на запись — без блокировки, через эксклюзивную ссылку.
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

    // Отравление (паника задачи посреди записи) не повод паниковать: ошибка этой задачи уже
    // возвращена из кадра, а решать, согласованы ли данные, — пользователю.
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

/// Ресурс, взятый на чтение.
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

/// Ресурс, взятый на запись.
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
