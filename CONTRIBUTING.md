# Как вносить изменения

## Перед PR

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
python tools/audit_workspace.py .
```

Если менялся `pragma_sync`, дополнительно:

```bash
cargo +nightly miri test -p pragma_sync
RUSTFLAGS="--cfg loom" cargo test -p pragma_sync --release --lib --tests
```

Если менялся `pragma_jobs` — долгие стресс-тесты (в CI — ночью) и бенчи против `rayon`:

```bash
cargo test -p pragma_jobs --release --test stress -- --ignored
cargo bench -p pragma_jobs
```

Если менялся `pragma_graph` — бенч кадра и fuzz построителя (cargo-fuzz, nightly, Linux;
отдельный workspace `fuzz/`). Тест «кадр без аллокаций» входит в обычный `cargo test`.

```bash
cargo bench -p pragma_graph
cargo +nightly fuzz run graph_build -- -max_total_time=60
```

## Контракт модуля

Новый крейт или модуль считается готовым, когда у него есть:

1. **Место в слое**: запись в `layers.toml`, зависимости только на нижние слои.
2. **Типизированная ошибка**: `#[non_exhaustive]` enum через `thiserror`, без `unwrap`/`expect`/`panic!`/индексации.
3. **Тесты**: unit-тесты и property-тесты на инварианты; loom-модель для атомарного протокола.
4. **Feature-флаг** в фасаде `pragma_core`, если модуль опционален.
5. **Документация**: `//!` в `lib.rs` с примером-doctest; `missing_docs` чист.
6. **Бенчмарк** (`criterion`), если модуль в горячем пути.

## Зависимости

- Только pure Rust (`deny.toml`). Перед добавлением выполните `cargo tree -e build -i cc` и прогоните аудит.
- Версия — только в `[workspace.dependencies]`, в крейте `{ workspace = true }`.
- Новая зависимость или исключение из правил — строка в `DECISIONS.md`.

## Коммиты

[Conventional Commits](https://www.conventionalcommits.org/): `feat(jobs): ...`, `fix(sync): ...`,
`docs: ...`, `ci: ...`, `refactor(graph): ...`. Ломающее изменение — `feat(jobs)!: ...` плюс запись в `CHANGELOG.md`.
