# Changelog

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.1.0/), версионирование — [SemVer](https://semver.org/lang/ru/).

## [Unreleased]

### Added
- Workspace по слоям: `pragma_base`, `pragma_sync`, `pragma_jobs`, `pragma_graph`, `pragma_async`, фасад `pragma_core`.
- `pragma_base`: `Handle`/`HandlePool` (generational), `Tick`, `BaseError`.
- `pragma_sync`: `CountLatch`; re-export `Backoff`, `CachePadded`.
- `pragma_jobs`: `PoolConfig` с валидацией, `JobsError`.
- CI (Windows + Linux), `cargo-deny`, аудит чистоты и слоёв, Miri и loom.
- Документация: архитектура, правила надёжности, roadmap, DECISIONS.
