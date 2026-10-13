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

- `#![forbid(unsafe_code)]` в каждом крейте, кроме `pragma_sync` (`layers.toml: unsafe_allowed`).
- В `pragma_sync`: `#![deny(unsafe_op_in_unsafe_fn)]`, `clippy::undocumented_unsafe_blocks = deny`,
  комментарий `// SAFETY:` на каждый блок с объяснением, почему инвариант выполняется.
- Тесты `pragma_sync` гоняются под Miri (`cargo +nightly miri test -p pragma_sync`).
- Каждый атомарный протокол (парковка, латчи, счётчики графа) получает loom-модель:
  `RUSTFLAGS="--cfg loom" cargo test -p pragma_sync --release --lib --tests` (модели — в
  `crates/pragma_sync/tests/loom.rs`). loom моделирует `SeqCst` как `AcqRel`, поэтому протоколы
  строятся на RMW-операциях, а не на «Dekker через SeqCst-load» (см. `AtomicCounter`).
- Новый `unsafe` вне `pragma_sync` — только через запись в `DECISIONS.md`. Единственное
  исключение — считающий аллокатор в тесте `pragma_graph/tests/alloc.rs` (DECISIONS #19).

## 4. Деградация вместо падения

| Сбой | Поведение |
|---|---|
| Паника в задаче | Перехват, ошибка из `join`/`scope`/хэндла; воркер жив |
| ОС не дала создать поток | `JobsError::SpawnFailed`; пул стартует с меньшим числом воркеров, если включено `allow_partial`, иначе ошибка |
| Цикл или неоднозначный порядок в графе | Ошибка `build()` с именами задач, до первого кадра |
| Ошибка или паника задачи графа | Её последователи в кадре пропускаются, независимые ветви доработают; `run` вернёт ошибку задачи с наименьшим индексом |
| Задача графа взяла необъявленный или занятый ресурс | `UndeclaredAccess` / `BorrowConflict` вместо гонки; слот, отравленный паникой, остаётся доступным |
| Drop хэндла async-задачи | Отмена без утечек |
| Зависание задачи | Watchdog в debug-сборках (M4): предупреждение в лог с именем задачи |

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
| Стресс / soak | Длительные тесты `#[ignore]` | Миллионы задач, вложенные `join`, случайные паники | M1 |
| Fuzz | `cargo-fuzz` (`fuzz/`, отдельный workspace) | Построитель графа на произвольных описаниях; тот же оракул — в proptest | M2 |
| Аллокации | Считающий глобальный аллокатор (`pragma_graph/tests/alloc.rs`) | Кадр графа без аллокаций | M2 |
| Детерминизм | Хеш состояния | 1/2/N воркеров | M5 |
| Производительность | `criterion` | spawn/join, `parallel_for`, граф; сравнение с `rayon` | M1; гейт регрессии с M5 |

## 7. CI

[`.github/workflows/ci.yml`](../.github/workflows/ci.yml):

1. `cargo fmt --check`
2. `cargo clippy --all-targets --all-features -D warnings` и `--no-default-features`
3. `cargo test --workspace --all-features` (Windows + Linux)
4. `cargo doc` без предупреждений
5. MSRV 1.88
6. `cargo deny check`: лицензии, advisories, запрещённые C-крейты
7. `tools/audit_workspace.py`: чистота и слои
8. Miri и loom для `pragma_sync`
9. `cargo hack clippy --each-feature`: каждая фича фасада собирается отдельно
10. Fuzz построителя графа, 60 с (`cargo +nightly fuzz run graph_build`, Linux)

Ночью ([`.github/workflows/nightly.yml`](../.github/workflows/nightly.yml)): стресс-тесты
`pragma_jobs` (`--ignored`) на Windows и Linux.

Добавляется по вехам: бенч-гейт и `cargo-semver-checks` (M5), долгий nightly-fuzz по
расписанию (M5).

## 8. Стабильность API

- Растущие enum/struct помечаются `#[non_exhaustive]`; вместо публичных полей — конструкторы и билдеры.
- Типы зависимостей (`crossbeam`, `async-task`) не торчат в публичном API, кроме осознанных re-export.
- Устаревание: `#[deprecated(since, note)]` минимум на один минорный релиз.
- До v1.0 допускаются ломающие изменения в минорных версиях (`0.x`), но каждое — в CHANGELOG.
