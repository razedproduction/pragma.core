# pragma_async

Part of [pragma.core](https://github.com/razedproduction/pragma.core), a pure-Rust multithreaded
core for game engines and frameworks (L3 layer).

Async tasks on the job pool (`Executor`, `AsyncHandle`) and a bounded blocking pool for synchronous I/O.

Most users should depend on the [`pragma_core`](https://crates.io/crates/pragma_core) facade
instead, which re-exports this crate. All `pragma_*` crates share one version and depend on each
other with `=X.Y.Z`: keep them at the same version.

License: MIT.
