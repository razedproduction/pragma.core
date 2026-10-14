# pragma_sync

Part of [pragma.core](https://github.com/razedproduction/pragma.core), a pure-Rust multithreaded
core for game engines and frameworks (L1 layer).

Synchronization primitives: `CountLatch`, `AtomicCounter`, task labels. The only crate of pragma.core with scheduler-level `unsafe`; its `__private` module is outside semver.

Most users should depend on the [`pragma_core`](https://crates.io/crates/pragma_core) facade
instead, which re-exports this crate. All `pragma_*` crates share one version and depend on each
other with `=X.Y.Z`: keep them at the same version.

License: MIT.
