# Правила надёжности

Надёжность здесь — это набор свойств, которые **проверяются машиной в CI**. Свойство, которое
нельзя проверить автоматически, со временем деградирует.

## 1. Паники

Запрещены линтами в рантайм-коде (`[workspace.lints.clippy]` в корневом `Cargo.toml`):
`unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `todo`, `unimplemented`, `dbg_macro`.
В тестах разрешены (`clippy.toml`).

- Вместо `v[i]` используем `v.get(i)` и обрабатываем `None`.
- Нарушение внутреннего инварианта (баг ядра) — это `debug_assert!` плюс безопасное поведение в release.
- `arithmetic_side_effects` — `warn` (в CI это ошибка из-за `-D warnings`): используем
  `checked_*`/`saturating_*`/`wrapping_*` осознанно.
- Отравленный мьютекс — не повод паниковать: `unwrap_or_else(PoisonError::into_inner)`, если
  защищаемые данные остаются согласованными.

## 2. Ошибки

- Один публичный `enum Error` на крейт, `#[non_exhaustive]`, через `thiserror`.
  Исключение: `pragma_jobs` — `JobsError` (пул) и `JobError` (исход задачи), DECISIONS #13.
- Ошибка несёт контекст: имя задачи, имя потока, индекс и поколение хэндла.
- `anyhow` — только в примерах и инструментах.
- Ошибки, повторяющиеся каждый кадр, логируются **один раз** (дедупликация по ключу).

## 3. `unsafe`

- `#![forbid(unsafe_code)]` в каждом крейте, кроме `pragma_sync` и `pragma_sys`
  (`layers.toml: unsafe_allowed`). В `pragma_sys` — только вызовы системных FFI (DECISIONS #27);
  Miri их не исполняет, крейт проверяется тестами на Windows и Linux в CI.
- В `pragma_sync`: `#![deny(unsafe_op_in_unsafe_fn)]`, `clippy::undocumented_unsafe_blocks = deny`,
  комментарий `// SAFETY:` на каждый блок с объяснением, почему инвариант выполняется.
- Тесты `pragma_sync` гоняются под Miri (`cargo +nightly miri test -p pragma_sync`).
- Каждый атомарный протокол (парковка, латчи, счётчики графа, латч `join_on_stack`) получает loom-модель:
  `RUSTFLAGS="--cfg loom" cargo test -p pragma_sync --release --lib --tests` (модели — в
  `crates/pragma_sync/tests/loom.rs`). loom моделирует `SeqCst` как `AcqRel`, поэтому протоколы
  строятся на RMW-операциях, а не на «Dekker через SeqCst-load» (см. `AtomicCounter`).
- Новый `unsafe` вне `pragma_sync` — только через запись в `DECISIONS.md`. Единственное
  исключение — считающий аллокатор в тесте `pragma_graph/tests/alloc.rs` (DECISIONS #19).
- **Счётчик `unsafe`** (DECISIONS #36): `[unsafe_budget]` в `layers.toml` — точное число
  вхождений ключевого слова по каждому файлу и решения, которые его разрешили. Аудит
  (`tools/audit_workspace.py`) падает при любом расхождении, а в PR рост бюджета файла без
  нового номера решения сверяется с базовой ревизией и роняет CI.

## 4. Деградация вместо падения

| Сбой | Поведение |
|---|---|
| Паника в задаче | Перехват, ошибка из `join`/`scope`/хэндла; воркер жив |
| ОС не дала создать поток | `JobsError::SpawnFailed`; пул стартует с меньшим числом воркеров, если включено `allow_partial`, иначе ошибка |
| Цикл или неоднозначный порядок в графе | Ошибка `build()` с именами задач, до первого кадра |
| Ошибка или паника задачи графа | Её последователи в кадре пропускаются, независимые ветви доработают; `run` вернёт ошибку задачи с наименьшим индексом |
| Задача графа взяла необъявленный или занятый ресурс | `UndeclaredAccess` / `BorrowConflict` вместо гонки; слот, отравленный паникой, остаётся доступным |
| Drop хэндла async-задачи | Отмена без утечек: future сбрасывается на исполнителе, невзятая blocking-задача не запускается |
| Паника в future или в `unblock` | `AsyncError::Panicked`; воркер и blocking-поток живы |
| Blocking-пул переполнен | `AsyncError::Overloaded` сразу, вызывающий (кадр) не ждёт |
| Остановка пула или исполнителя при живых async-задачах | Задачи отменяются, ожидающие получают `Cancelled`, а не зависают |
| `wait()` async-хэндла из воркера или кадра графа | `AsyncError::WouldBlock` вместо дедлока (DECISIONS #25) |
| Зависание задачи | Watchdog в debug-сборках: одно предупреждение `pragma_jobs::watchdog` с именем задачи и потока (`PoolConfig::with_watchdog`, 5 с); в release кода нет |
| ОС отказала в приоритете или привязке потока | Предупреждение в лог, воркер работает без настройки (`SysError` из `pragma_sys`) |

### Что ядро не гарантирует

Эти сбои внутри процесса не перехватываются. Реакция на них (крэш-репортер, минидампы,
автосохранение, перезапуск супервизором) — ответственность приложения или движка, а не ядра.

| Сбой | Почему |
|---|---|
| Паника при `panic = "abort"` в профиле приложения | `catch_unwind` не работает, процесс завершается (DECISIONS #9) |
| Паника во время ожидания в `scope`/`join` | Намеренный `abort`: иначе заимствованные задачами данные стали бы висячими (DECISIONS #15) |
| Переполнение стека | Rust завершает процесс, перехват невозможен |
| Нехватка памяти | Стандартный аллокатор вызывает `handle_alloc_error` → `abort` |
| Ошибка доступа к памяти, сигнал ОС | Вне модели Rust; при отсутствии `unsafe`-багов в `pragma_sync` не возникает |

Что ядро даёт приложению для этих случаев: контекст текущей задачи на потоке —
`pragma_jobs::current_task(|t| …)` без аллокаций и блокировок (DECISIONS #28); его можно
прочитать из panic hook или обработчика падений, чтобы назвать виновника в отчёте (тест
`pragma_core/tests/crash_context.rs`, без аллокаций — `pragma_graph/tests/alloc.rs`); детерминизм
(§5) — чтобы падение воспроизвести повтором сценария.

## 5. Детерминизм

Желателен сейчас и **обязателен к v1.0** (DECISIONS #3). Подробности — в
[architecture.md §6](architecture.md#6-детерминизм). Тест: один и тот же сценарий на 1, 2 и N
воркерах даёт одинаковый хеш.

## 6. Пирамида тестов

| Уровень | Инструмент | Что покрывает | С какой вехи |
|---|---|---|---|
| Unit | `cargo test` | Логика модулей | M0 |
| Property | `proptest` | Хэндлы, графы (случайные DAG → валидный порядок), `parallel_for` = последовательному | M0 |
| Doctest | rustdoc | Пример в `//!` каждого крейта компилируется и работает | M0 |
| Модель-чекинг | `loom` | Атомарные протоколы `pragma_sync` | M1 |
| UB | Miri | `pragma_sync`, а также тесты `pragma_jobs` в однопоточном режиме | M1 |
| Стресс / soak | Длительные тесты `#[ignore]` | Миллионы задач, вложенные `join`, случайные паники; async-задачи со случайной отменой (M3) | M1 |
| Fuzz | `cargo-fuzz` (`fuzz/`, отдельный workspace) | Построитель графа на произвольных описаниях; тот же оракул — в proptest | M2 |
| Аллокации | Считающий глобальный аллокатор (`pragma_graph/tests/alloc.rs`) | Кадр графа без аллокаций (с M4 — и с подписчиком `tracing`, и чтение метки из panic hook) | M2 |
| Детерминизм | Хеш состояния (`frame_sim --suite`, `frame_sim/tests/suite.rs`) | 5 сценариев × 1/2/N воркеров × мгновенная/запаздывающая загрузка; `idle_spin`, `PoolStats`, watchdog, подписчик `tracing` не меняют хеш (DECISIONS #35) | M3; обязательный набор — M5 |
| Производительность | `criterion` + `tools/bench_gate.py` | spawn/join, `parallel_for`, граф, `frame_sim` мс/тик; сравнение с `rayon`; A/B-гейт против `main` (DECISIONS #34) | M1; гейт регрессии с M5 |
| API | `cargo-semver-checks` | Ломающее изменение без подъёма версии (baseline — `main`, DECISIONS #33) | M5 |
| Бюджет `unsafe` | `tools/audit_workspace.py` | Точное число `unsafe` по файлам, рост — только с записью в DECISIONS (#36) | M5 |

## 7. CI

[`.github/workflows/ci.yml`](../.github/workflows/ci.yml):

1. `cargo fmt --check`
2. `cargo clippy --all-targets --all-features -D warnings` и `--no-default-features`
3. `cargo test --workspace --all-features` (Windows + Linux)
4. `cargo doc` без предупреждений
5. MSRV 1.88
6. `cargo deny check`: лицензии, advisories, запрещённые C-крейты
7. `tools/audit_workspace.py`: чистота, слои и бюджет `unsafe` (в PR — против базовой ревизии)
8. Miri и loom для `pragma_sync`
9. `cargo hack clippy --each-feature`: каждая фича фасада собирается отдельно
10. Fuzz построителя графа, 60 с (`cargo +nightly fuzz run graph_build`, Linux)
11. Набор детерминизма: `cargo run -p frame_sim --release -- --suite --ticks 200` (Windows + Linux;
    debug-вариант — `frame_sim/tests/suite.rs` в шаге 3)
12. Watchdog в release не срабатывает: `cargo test -p pragma_jobs --release --test watchdog` (Windows + Linux)
13. `cargo semver-checks --workspace --baseline-rev <base>` (PR, Windows + Linux)
14. Бенч-гейт `tools/bench_gate.py` (PR, Windows + Linux): A/B против `main`, регрессия > 10%
    сверх шума эталона роняет PR; методика — [perf.md §6](perf.md#6-бенч-гейт-в-ci)

Ночью ([`.github/workflows/nightly.yml`](../.github/workflows/nightly.yml)), Windows и Linux:
soak — стресс-тесты `pragma_jobs` и `pragma_async` (`--ignored`) в 5 раз длиннее с новым seed
каждую ночь (`PRAGMA_SOAK_ROUNDS`, `PRAGMA_SOAK_SEED`), набор детерминизма на 3000 тиков;
Linux — fuzz построителя графа 30 минут с корпусом, переживающим ночи (`actions/cache`), находки —
артефактом.

## 8. Стабильность API

- Растущие enum/struct помечаются `#[non_exhaustive]`; вместо публичных полей — конструкторы и билдеры.
- Типы зависимостей (`crossbeam`, `async-task`) не торчат в публичном API, кроме осознанных re-export.
- Устаревание: `#[deprecated(since, note)]` минимум на один минорный релиз.
- До v1.0 допускаются ломающие изменения в минорных версиях (`0.x`), но каждое — в CHANGELOG.
