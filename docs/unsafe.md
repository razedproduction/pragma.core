# Ревью `unsafe` (веха M5)

Все вхождения ключевого слова `unsafe` в коде workspace — 52 в 7 файлах. Сверено с
`[unsafe_budget]` в `layers.toml`: расхождение роняет аудит, а рост бюджета без записи в
DECISIONS — CI (DECISIONS #36). На каждое вхождение — чек-лист: **инвариант → кто его
гарантирует → чем проверено**. Состояние на 2026-10-14 (ветка M5b).

Чем проверяется:

| Метка | Что это |
|---|---|
| Miri-SB | `cargo +nightly miri test -p pragma_sync` (Stacked Borrows, CI `miri`) |
| Miri-TB | `cargo +nightly miri test -p pragma_jobs --test miri` с `-Zmiri-tree-borrows`: пул из 1 воркера поверх всего `unsafe` `pragma_sync` (CI `miri`, DECISIONS #38) |
| loom | модели `crates/pragma_sync/tests/loom.rs` (CI `loom`) |
| ОС-тест | тесты `pragma_sys` на Windows и Linux (CI `check`); Miri FFI не исполняет |
| тест | обычные тесты, в том числе `tests/alloc.rs`, `tests/oom.rs` |

## `pragma_sync/src/scope.rs` — 9 (DECISIONS #15)

Общий инвариант: `scope()` не возвращается ни нормально, ни паникой, пока счётчик `pending`
не обнулился. Паника в ожидании ведёт к `abort`. Каждая задача уменьшает счётчик ровно один
раз (`DoneGuard`), уже *после* того, как её замыкание выполнено или уничтожено (поле `_done`
объявлено после `body`).

| # | Где | Инвариант | Кто гарантирует | Проверено |
|---|---|---|---|---|
| 1 | `unsafe impl<F: Send> Sync for StackSlot<F>` | к `f` обращается только задача, получившая слот | `claimed.swap(true)` в `task_in` отдаёт слот одной задаче; у владельца доступа к `f` нет | Miri-SB (`scope` тесты), Miri-TB |
| 2 | поле `call: unsafe fn(*const (), bool)` | вызывается только с указателем на `StackSlot<F>` того же `F` | `task_in` кладёт пару `(slot, call_slot::<F>)` вместе, поля приватны | компилятор (мономорфизация), Miri-SB |
| 3 | `TaskBody::drop` → `call(slot, false)` | слот жив, вызов однократный | слот `&'scope`, область ждёт задачу; `pending` сбрасывается до вызова | Miri-SB (`dropped`/отмена), Miri-TB |
| 4 | `unsafe fn call_slot` | контракт: живой слот, доступ — у вызывающей задачи | вызывается только из 3 и 8 | Miri-SB |
| 5 | `&*ptr.cast::<StackSlot<F>>()` | указатель на живой `StackSlot<F>` | контракт 4 | Miri-SB |
| 6 | `(*slot.f.get()).take()` | других ссылок на содержимое ячейки нет | однократный вызов (3/8) у единственной задачи (1) | Miri-SB |
| 7 | `unsafe impl Send for ScopedTask` | замыкание `Send`, указатель на слот передаёт только эксклюзивный доступ | `task`/`task_in` требуют `F: Send` | компилятор, Miri-TB (задачи на воркере) |
| 8 | `ScopedTask::run` → `call(slot, true)` | как 3 | `pending = false` до вызова: при панике `drop` слот не трогает | Miri-SB, стресс с паниками |
| 9 | `transmute` `Box<dyn FnOnce + 'scope>` → `'static` | замыкание не переживёт `'scope` | общий инвариант; раскладка `Box<dyn>` не меняется (только лайфтайм) | Miri-SB, Miri-TB (`scope` с заимствованием стека), proptest вложенных `scope` |

## `pragma_sync/src/join.rs` — 9 (DECISIONS #30)

Общий инвариант: `join_on_stack` не возвращается, пока латч не `SET`. Паника в ожидании ведёт
к `abort`. Исполнитель после `swap`/`store` латча кадр не трогает: `owner` прочитан заранее,
результат записан до установки латча.

| # | Где | Инвариант | Кто гарантирует | Проверено |
|---|---|---|---|---|
| 1 | поле `call: unsafe fn(*const (), Mode)` | пара `(frame, call::<F, R>)` от одного кадра | создаётся только в `join_on_stack`, поля приватны | компилятор, Miri-SB |
| 2 | `unsafe impl Send for StackJob` | замыкание и результат `Send`, латч атомарный | `join_on_stack` требует `B: Send`, `RB: Send` | компилятор, Miri-TB (вор на другом потоке) |
| 3 | `run` → `call(Thief)` | кадр жив, вызов однократный | общий инвариант; `pending = false` (`self` по значению) | Miri-SB (`thief_runs_on_other_thread`), loom |
| 4 | `run_inline` → `call(Owner)` | как 3 | как 3 | Miri-SB (`inline_run_needs_no_wait`) |
| 5 | `Drop` → `call(Cancel)` | как 3; паника деструктора не обходит латч | `catch_unwind` вокруг `drop(f)` | Miri-SB (`dropped_job_is_cancelled`) |
| 6 | `unsafe fn call` | контракт: живой кадр, первый и единственный вызов | вызывается только из 3–5 | Miri-SB |
| 7 | `&*ptr.cast::<Frame<F, R>>()` | указатель на живой кадр | контракт 6 | Miri-SB, Miri-TB |
| 8 | `(*frame.f.get()).take()` | единственный доступ к `f` | контракт 6 | Miri-SB |
| 9 | `*frame.result.get() = Some(..)` | владелец читает результат только после `SET` (Acquire) | запись до `store(Release)` / `swap(AcqRel)` | loom `stack_join_owner_never_misses_thief`, Miri-SB |

Замечание ревью. После `swap(SET)` владелец может сразу уничтожить кадр, пока вызов `swap`
ещё формально держит `&AtomicUsize`. Это тот же паттерн, что `Arc::drop` в std и латчи rayon:
после последней атомарной операции ссылка не используется, и обе модели Miri (SB и TB) его
принимают. Изменения не требуется. Пересмотреть, если модель памяти Rust запретит
освобождение памяти под живой `&UnsafeCell`-ссылкой.

## `pragma_sync/src/label.rs` — 3 (DECISIONS #28)

Общий инвариант: `CURRENT` указывает либо в никуда, либо на `Frame` в стеке ещё не вернувшегося
вызова `run_labeled` *этого же потока*. Кадры ставятся и снимаются строго по стеку (LIFO,
`Restore` срабатывает и при раскрутке).

| # | Где | Инвариант | Кто гарантирует | Проверено |
|---|---|---|---|---|
| 1 | `current_label`: `frame.as_ref()` | кадр жив всё время `f` | общий инвариант; `TaskLabel<'a>` не выходит из `f` (HRTB) и `!Send` (сырые указатели) | Miri-SB (`label` тесты, паника внутри метки), Miri-TB (`labels_name_the_running_task`) |
| 2 | `TaskLabel::name`: `&*frame.name` | строка жива на `'a` | `name: &str` заимствован вызовом `run_labeled`, который владеет кадром | Miri-SB |
| 3 | `TaskLabel::parent`: `parent.as_ref()` | внешний кадр живёт дольше внутреннего | LIFO по стеку одного потока | Miri-SB (`nested_labels_form_a_chain_and_unwind`) |

## `pragma_sys/src/windows.rs` — 6 (DECISIONS #27)

Все вызовы — Win32 через `windows-sys`, на псевдохэндле `GetCurrentThread()`: его не нужно
закрывать, у него есть права на запрос и изменение информации потока.

| # | Вызов | Инвариант | Кто гарантирует | Проверено |
|---|---|---|---|---|
| 1 | `GetLastError` | без предусловий | — | ОС-тест (ошибочные пути) |
| 2 | `SetThreadDescription(h, wide)` | `wide` — UTF-16 с нулём, живёт до конца вызова | локальный `Vec` с `push(0)`; длина ≤ 256 + 1 (M5b) | ОС-тест `name_is_applied` |
| 3 | `SetThreadPriority` | значение — документированная константа | `match` по `ThreadPriority` | ОС-тест `lower_priority_reads_back` |
| 4 | `GetThreadPriority` | ошибка — `THREAD_PRIORITY_ERROR_RETURN` | проверка возврата | ОС-тест |
| 5 | `SetThreadAffinityMask` | маска с одним битом, ядро < 64 | проверка `core < MAX_CORES` и `checked_shl` | ОС-тест `pinned_thread_runs_on_its_core`, `out_of_range_core_is_an_error` |
| 6 | `GetCurrentProcessorNumber` | без предусловий | — | ОС-тест |

## `pragma_sys/src/linux.rs` — 9 (DECISIONS #27)

| # | Вызов | Инвариант | Кто гарантирует | Проверено |
|---|---|---|---|---|
| 1 | `pthread_setname_np(pthread_self(), c)` | C-строка ≤ 16 байт с нулём | обрезка до 15 байт по границе символа, `CString::new` | ОС-тест (Linux CI) |
| 2 | `gettid` | без предусловий | — | ОС-тест |
| 3 | `setpriority(PRIO_PROCESS, tid, nice)` | меняется только `nice` текущего потока | `tid()` текущего потока | ОС-тест |
| 4 | `*__errno_location() = 0` | указатель на errno текущего потока валиден | glibc | ОС-тест |
| 5 | `getpriority` | только чтение; −1 отличается от ошибки по errno | errno сброшен в 4 | ОС-тест |
| 6 | `mem::zeroed::<cpu_set_t>()` | нулевой образ — корректное пустое множество | `cpu_set_t` — массив целых | ОС-тест |
| 7 | `CPU_SET(core, &mut set)` | бит внутри `set` | проверка `core < CPU_SETSIZE` | ОС-тест `out_of_range_core_is_an_error` |
| 8 | `sched_setaffinity(0, size, &set)` | размер и указатель — от локального `set` | `size_of::<cpu_set_t>()` | ОС-тест |
| 9 | `sched_getcpu` | без предусловий | — | ОС-тест |

Замечание ревью. `tid()` при недопустимом tid вернул бы 0, а `who = 0` в `setpriority` означает
«вызывающий». Недостижимо: `gettid` всегда положителен. Оставлено как есть.

Linux-код на Windows-машине проверяется кросс-clippy:
`cargo clippy --target x86_64-unknown-linux-gnu -p pragma_sys --lib`.

## Тестовые аллокаторы — `pragma_graph/tests/alloc.rs` (9, DECISIONS #19), `pragma_base/tests/oom.rs` (7, DECISIONS #37)

| Где | Инвариант | Кто гарантирует | Проверено |
|---|---|---|---|
| `unsafe impl GlobalAlloc` | контракт `GlobalAlloc` соблюдён | все методы делегируют `System` с теми же аргументами; `oom.rs` вместо выделения возвращает null, что контракт разрешает | тесты |
| `unsafe fn alloc/alloc_zeroed/realloc/dealloc` | контракт передаётся вызывающим | сигнатура трейта | тесты |
| блоки `System.*` | `ptr` выделен `System` через этот же аллокатор | глобальный аллокатор бинарника один | тесты (`tests/alloc.rs` — 0 аллокаций за 2000 кадров; `tests/oom.rs` — `OutOfMemory` без `abort`) |

`oom.rs` не переопределяет `alloc_zeroed`: реализация по умолчанию вызывает `alloc` и поэтому
тоже отказывает.

## Итог

- Ошибок не найдено. Два замечания (рядом с `join.rs` и `linux.rs`) — без изменения кода.
- Каждое вхождение покрыто Miri, loom или тестом на реальной ОС. FFI Miri не исполняет, и
  `pragma_jobs` под Miri этих путей не касается: пул без `with_thread_priority`/`with_affinity`
  не вызывает `pragma_sys`, а имена потоков задаёт `std::thread::Builder`.
- Новый `unsafe` — только с записью в DECISIONS и правкой бюджета (reliability §3).
