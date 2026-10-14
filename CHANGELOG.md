# Changelog

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.1.0/), версионирование — [SemVer](https://semver.org/lang/ru/).

## [Unreleased]

### Added — M4a «Наблюдаемость»
- Новый крейт `pragma_sys` (L1, `unsafe` только для системных FFI, DECISIONS #27):
  `set_current_thread_name`, `set_current_thread_priority` / `current_thread_priority`
  (`ThreadPriority::{Low, Normal, High}`), `pin_current_thread`, `current_core`; ошибки —
  `SysError`. Доступен как `pragma_jobs::sys` и `pragma_core::sys`.
- `PoolConfig::with_thread_priority`, `with_affinity` (воркер `i` → `cores[i % len]`): отказ
  ОС — предупреждение в лог, воркер работает без настройки.
- Контекст текущей задачи для крэш-репортов (DECISIONS #28): `pragma_sync::run_labeled` /
  `current_label` / `TaskLabel`, в `pragma_jobs` — `labeled(kind, name, f)` и `current_task(f)`.
  Метки у задач графа, `spawn`, задач `scope`/`parallel_*`, `spawn_detached`, async и `unblock`;
  чтение — без аллокаций, годится для panic hook.
- `ThreadPool::spawn_named`, `Executor::spawn_async_named`: имя задачи в метке и в ошибке паники.
- `ThreadPool::stats() -> PoolStats` (`ThreadStats` по воркерам и внешним потокам): выполнено,
  украдено, парковки, время простоя и сна, глубина очередей. Без блокировок и RMW на горячем пути.
- Watchdog зависших задач (только debug): `PoolConfig::with_watchdog(Option<Duration>)`, по
  умолчанию `DEFAULT_WATCHDOG` = 5 с; одно `tracing::warn!` (target `pragma_jobs::watchdog`) на
  задачу. В release кода нет.
- `tracing`: span `graph_task` (задача, приоритет, воркер) и `frame_graph` — `DEBUG`;
  `scope`/`parallel_for`/`parallel_reduce` — `TRACE`.
- Feature `puffin` (`pragma_jobs`, `pragma_graph`, фасад; puffin 0.19.1, DECISIONS #29):
  области профайлера на кадр, задачи графа и операции пула; фасад re-export-ит `puffin`.
- `ThreadPool::current_worker_index`, `pragma_jobs::current_priority`, `Priority::as_str`.
- Тесты: `pragma_jobs/tests/observability.rs`, `tests/watchdog.rs` (debug и release),
  `pragma_core/tests/crash_context.rs` (panic hook называет задачу графа / spawn / async /
  unblock); `pragma_graph/tests/alloc.rs` — кадр без аллокаций и с подписчиком `tracing`, чтение
  метки из panic hook без аллокаций.

### Changed — M4a
- `JobError` отменённой/упавшей задачи `spawn_named` несёт её имя вместо `spawn`.

### Added — M3 «Async-задачи и вертикальный срез»
- `pragma_async`: `Executor::new(&pool, AsyncConfig)` — исполнитель на `async-task` поверх пула.
  `spawn_async(fut) -> AsyncHandle<T>`: future исполняется на воркерах с приоритетом
  `Background`; паника — `AsyncError::Panicked { task, message }`.
- `Executor::unblock(f)`: синхронный I/O в ограниченном blocking-пуле (потоки по требованию,
  `AsyncConfig::with_blocking_threads` / `with_blocking_capacity`); переполнение — сразу
  `AsyncError::Overloaded`, вызывающий не ждёт (DECISIONS #23).
- `AsyncHandle`: отмена сбросом, `detach()`, мост в кадр `poll_ready()`, `Future` для `.await`,
  `wait()` вне работы пула (из воркера или кадра графа — `AsyncError::WouldBlock`, DECISIONS #25).
- `AsyncError`: варианты `Overloaded`, `SpawnFailed`, `WouldBlock`, `Consumed`, `InvalidConfig`.
- `pragma_jobs::Spawner` (`ThreadPool::spawner()`): `Weak`-ссылка на пул, `spawn_detached`,
  `is_alive`, `in_pool_context` (DECISIONS #24).
- Пример `examples/frame_sim` (член workspace, слой 5): 100k сущностей, 9 систем графа,
  фоновая загрузка карты через `spawn_async` + `unblock`, хеш состояния на каждом тике;
  `--verify 1,4` сверяет хеши на разном числе воркеров и времени загрузки (DECISIONS #26).
- Тесты: `pragma_async/tests/executor.rs` (отмена на всех этапах со счётчиком `Drop`, паники,
  blocking-пул, остановка пула/исполнителя), proptest и `#[ignore]`-стресс (`tests/prop.rs`);
  `frame_sim/tests/determinism.rs`.
- CI: шаг `frame_sim --verify 1,4` на Windows и Linux; ночной стресс `pragma_async`.
- Фасад: `AsyncConfig`, `AsyncError`, `AsyncHandle`, `Executor` в `prelude` (feature `async`).

### Changed — M3
- `pragma_async::AsyncError::Panicked` получил поле `task` (контекст: `spawn_async` / `unblock`).
- `pragma_jobs`: деструктор задачи, сброшенной без выполнения (остановка пула), больше не может
  уронить останавливающий поток паникой — она перехватывается и пишется в лог.

### Added — M2 «Граф кадра»
- `pragma_graph`: `FrameGraph::builder()` — задачи с `reads::<T>()` / `writes::<T>()` и явными
  `before`/`after`; режим `ordered` (конфликт упорядочивается порядком объявления).
- `build()` до первого кадра: `GraphError::DuplicateTask`, `UnknownDependency`, `Cycle` (с
  именами задач цикла), `AmbiguousOrder` (конфликт доступа без порядка, с ресурсом).
- Стабильная топосортировка (ничьи — по порядку регистрации) и транзитивная редукция рёбер;
  `FrameGraph::order()`, `edges()`.
- `FrameGraph::run(&pool, &mut resources)`: dataflow-исполнение без аллокаций в установившемся
  режиме; независимые задачи — параллельно. Ошибка/паника задачи (`TaskFailed`,
  `TaskPanicked`) пропускает её последователей, не останавливая независимые ветви; возвращается
  ошибка задачи с наименьшим индексом.
- `Resources`: типизированные слоты (`RwLock` на тип) с `insert`/`remove`/`get`/`get_mut`;
  `TaskContext::read`/`write` проверяют объявленный доступ (`UndeclaredAccess`,
  `BorrowConflict`, `MissingResource`). Граф не владеет ресурсами — можно исполнять над
  несколькими экземплярами.
- Вложенный параллелизм: `TaskContext::pool()` для `parallel_for`/`join`/`scope` из задачи.
- Экспорт графа: `to_dot()` (Graphviz) и `to_mermaid()`.
- `TaskResult`: система возвращает `()` или `Result<(), E>`; `GraphError` из `?` пробрасывается как есть.
- `pragma_sync::ArrivalCounter` — «последний из N прибывших продолжает» (+ loom-модель).
- Тесты: поведенческие (`tests/graph.rs`), «кадр без аллокаций» со считающим аллокатором
  (`tests/alloc.rs`), proptest против независимого оракула (`tests/prop.rs`, `tests/oracle`),
  fuzz построителя (`fuzz/`, cargo-fuzz). Бенч кадра: `cargo bench -p pragma_graph`.
- CI: `cargo hack clippy --each-feature`, fuzz графа 60 с.
- Фасад: `FrameGraph`, `GraphError`, `Resources`, `TaskContext` в `prelude` (feature `graph`).

### Changed — M2
- `pragma_jobs`: внешний поток переиспользует кэшированный контекст слота пула — вход в пул
  (например, раз в кадр) больше не аллоцирует (DECISIONS #18).
- `pragma_sync`: кэш счётчиков областей возвращает счётчик при нуле, даже если клон `Arc` ещё
  жив у завершающейся задачи — промахи кэша больше не аллоцируют.
- `GraphError` получил варианты `DuplicateTask`, `MissingResource`, `UndeclaredAccess`,
  `BorrowConflict`, `TaskFailed`, `TaskPanicked`, `Job` (enum `#[non_exhaustive]`).

### Added — M1 «Job system»
- `pragma_jobs`: `ThreadPool::new(PoolConfig) -> Result` с именованными потоками (`<prefix>-<index>`)
  и `Drop`, который останавливает и `join`-ит все потоки (`ShutdownPolicy::{Drain, Cancel}`).
- Work-stealing: локальные LIFO-очереди `crossbeam_deque::Worker` по приоритетам, глобальные
  `Injector`, кража у случайной жертвы; адресное пробуждение одного спящего воркера.
- `spawn(Priority, f) -> JobHandle<T>`, `JobHandle::join() -> Result<T, JobError>`.
- `join(a, b)`, `scope(|s| ...)` с заимствованием данных, `parallel_for`, `parallel_for_chunks`,
  `parallel_reduce` (детерминированная свёртка в порядке чанков), `Grain::{Auto, Fixed(n)}`.
- Help-while-wait во всех ожиданиях: вложенные `join`/`scope` не дедлочат даже на одном воркере;
  внешний поток (главный) участвует в работе через внешние слоты пула.
- Перехват паник: `JobError::Panicked { job, message }`, `JobError::Cancelled { job }`.
- Приоритеты `Frame` / `Normal` / `Background`, лимит воркеров для `Background`
  (`PoolConfig::with_background_limit`).
- `pragma_sync`: `Parker`/`Unparker`, `AtomicCounter` (+ `wait_while_helping`), `scope()` /
  `TaskScope` / `ScopedTask` (стирание лайфтайма задач области), `StackSlot` (задача без
  аллокации), `SyncError`.
- loom-модели латча, парковки и счётчика (`crates/pragma_sync/tests/loom.rs`).
- Тесты: поведенческие (`tests/pool.rs`), proptest (`parallel_for`/`parallel_reduce` равны
  последовательному коду, вложенные `scope` случайной глубины), стресс `#[ignore]`
  (10⁷ задач, случайные паники, 1..2N воркеров) и ночной workflow для них.
- Бенчи `criterion` против `rayon`: `cargo bench -p pragma_jobs`.

### Changed
- `pragma_sync::CountLatch::new` больше не `const fn` (нужно для loom-моделей).
- `loom` подключается к `pragma_sync` как `[target.'cfg(loom)'.dependencies]`;
  loom-джоб CI запускает `--lib --tests` (DECISIONS #12).
- `deny.toml`: граф ограничен целевыми платформами Windows/Linux x86_64.

### Added — M0
- Workspace по слоям: `pragma_base`, `pragma_sync`, `pragma_jobs`, `pragma_graph`, `pragma_async`, фасад `pragma_core`.
- `pragma_base`: `Handle`/`HandlePool` (generational), `Tick`, `BaseError`.
- `pragma_sync`: `CountLatch`; re-export `Backoff`, `CachePadded`.
- `pragma_jobs`: `PoolConfig` с валидацией, `JobsError`.
- CI (Windows + Linux), `cargo-deny`, аудит чистоты и слоёв, Miri и loom.
- Документация: архитектура, правила надёжности, roadmap, DECISIONS.
