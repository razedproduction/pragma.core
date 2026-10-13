## Что и зачем

<!-- Кратко: что меняется и какая веха roadmap / задача закрывается. -->

## Чек-лист

- [ ] `cargo fmt`, `clippy -D warnings`, `cargo test` проходят локально
- [ ] `python tools/audit_workspace.py .` чист
- [ ] Новые публичные элементы задокументированы, в `lib.rs` есть doctest
- [ ] Тесты: unit / property / loom (для атомарного кода)
- [ ] `unsafe` только в `pragma_sync`, каждый блок с `// SAFETY:`
- [ ] Новая зависимость или исключение записаны в `DECISIONS.md`
- [ ] `CHANGELOG.md` обновлён (для заметных изменений)
