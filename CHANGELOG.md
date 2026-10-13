# Changelog

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.1.0/), версионирование — [SemVer](https://semver.org/lang/ru/).

## [Unreleased]

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
