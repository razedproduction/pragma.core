# `unsafe` review (milestone M5)

All occurrences of the `unsafe` keyword in workspace code: 52 in 7 files. Cross-checked against
`[unsafe_budget]` in `layers.toml`: a mismatch fails the audit, and budget growth without a
DECISIONS entry fails CI (DECISIONS #36). For every occurrence, a checklist: **invariant → who
guarantees it → how it is checked**. State as of 2026-10-14 (the M5b branch).

How it is checked:

| Tag | What it is |
|---|---|
| Miri-SB | `cargo +nightly miri test -p pragma_sync` (Stacked Borrows, CI `miri`) |
| Miri-TB | `cargo +nightly miri test -p pragma_jobs --test miri` with `-Zmiri-tree-borrows`: a 1-worker pool over all of `pragma_sync`'s `unsafe` (CI `miri`, DECISIONS #38) |
| loom | the models in `crates/pragma_sync/tests/loom.rs` (CI `loom`) |
| OS test | `pragma_sys` tests on Windows and Linux (CI `check`); Miri does not execute FFI |
| test | regular tests, including `tests/alloc.rs`, `tests/oom.rs` |

## `pragma_sync/src/scope.rs` — 9 (DECISIONS #15)

The shared invariant: `scope()` returns neither normally nor by panic until the `pending` counter
has reached zero. A panic while waiting leads to `abort`. Every task decrements the counter exactly once
(`DoneGuard`), and only *after* its closure has run or been destroyed (the `_done` field
is declared after `body`).

| # | Where | Invariant | Who guarantees it | Checked by |
|---|---|---|---|---|
| 1 | `unsafe impl<F: Send> Sync for StackSlot<F>` | only the task that got the slot accesses `f` | `claimed.swap(true)` in `task_in` hands the slot to one task; the owner has no access to `f` | Miri-SB (`scope` tests), Miri-TB |
| 2 | the field `call: unsafe fn(*const (), bool)` | called only with a pointer to a `StackSlot<F>` of the same `F` | `task_in` stores the pair `(slot, call_slot::<F>)` together, the fields are private | the compiler (monomorphization), Miri-SB |
| 3 | `TaskBody::drop` → `call(slot, false)` | the slot is alive, a single call | the slot is `&'scope`, the scope waits for the task; `pending` is cleared before the call | Miri-SB (`dropped`/cancellation), Miri-TB |
| 4 | `unsafe fn call_slot` | contract: a live slot, access belongs to the calling task | called only from 3 and 8 | Miri-SB |
| 5 | `&*ptr.cast::<StackSlot<F>>()` | a pointer to a live `StackSlot<F>` | contract 4 | Miri-SB |
| 6 | `(*slot.f.get()).take()` | there are no other references to the cell's contents | a single call (3/8) by the only task (1) | Miri-SB |
| 7 | `unsafe impl Send for ScopedTask` | the closure is `Send`; the slot pointer hands over only exclusive access | `task`/`task_in` require `F: Send` | the compiler, Miri-TB (tasks on a worker) |
| 8 | `ScopedTask::run` → `call(slot, true)` | as 3 | `pending = false` before the call: on panic, `drop` does not touch the slot | Miri-SB, stress with panics |
| 9 | `transmute` of `Box<dyn FnOnce + 'scope>` → `'static` | the closure will not outlive `'scope` | the shared invariant; the `Box<dyn>` layout does not change (only the lifetime) | Miri-SB, Miri-TB (`scope` borrowing the stack), proptest of nested `scope`s |

## `pragma_sync/src/join.rs` — 9 (DECISIONS #30)

The shared invariant: `join_on_stack` does not return until the latch is `SET`. A panic while waiting leads
to `abort`. After the `swap`/`store` of the latch the executor does not touch the frame: `owner` is read beforehand,
and the result is written before the latch is set.

| # | Where | Invariant | Who guarantees it | Checked by |
|---|---|---|---|---|
| 1 | the field `call: unsafe fn(*const (), Mode)` | the pair `(frame, call::<F, R>)` comes from one frame | created only in `join_on_stack`, the fields are private | the compiler, Miri-SB |
| 2 | `unsafe impl Send for StackJob` | the closure and the result are `Send`, the latch is atomic | `join_on_stack` requires `B: Send`, `RB: Send` | the compiler, Miri-TB (a thief on another thread) |
| 3 | `run` → `call(Thief)` | the frame is alive, a single call | the shared invariant; `pending = false` (`self` by value) | Miri-SB (`thief_runs_on_other_thread`), loom |
| 4 | `run_inline` → `call(Owner)` | as 3 | as 3 | Miri-SB (`inline_run_needs_no_wait`) |
| 5 | `Drop` → `call(Cancel)` | as 3; a destructor panic does not bypass the latch | `catch_unwind` around `drop(f)` | Miri-SB (`dropped_job_is_cancelled`) |
| 6 | `unsafe fn call` | contract: a live frame, the first and only call | called only from 3–5 | Miri-SB |
| 7 | `&*ptr.cast::<Frame<F, R>>()` | a pointer to a live frame | contract 6 | Miri-SB, Miri-TB |
| 8 | `(*frame.f.get()).take()` | the only access to `f` | contract 6 | Miri-SB |
| 9 | `*frame.result.get() = Some(..)` | the owner reads the result only after `SET` (Acquire) | written before `store(Release)` / `swap(AcqRel)` | loom `stack_join_owner_never_misses_thief`, Miri-SB |

Review remark. After `swap(SET)` the owner may destroy the frame immediately, while the `swap` call
still formally holds a `&AtomicUsize`. This is the same pattern as `Arc::drop` in std and rayon's latches:
the reference is not used after the last atomic operation, and both Miri models (SB and TB)
accept it. No change is needed. Revisit if the Rust memory model forbids
freeing memory under a live `&UnsafeCell` reference.

## `pragma_sync/src/label.rs` — 3 (DECISIONS #28)

The shared invariant: `CURRENT` is either null or points to a `Frame` on the stack of a not yet returned
`run_labeled` call *of the same thread*. Frames are set and cleared strictly in stack order (LIFO,
`Restore` fires during unwinding too).

| # | Where | Invariant | Who guarantees it | Checked by |
|---|---|---|---|---|
| 1 | `current_label`: `frame.as_ref()` | the frame is alive for the whole of `f` | the shared invariant; `TaskLabel<'a>` does not escape `f` (HRTB) and is `!Send` (raw pointers) | Miri-SB (`label` tests, a panic inside a label), Miri-TB (`labels_name_the_running_task`) |
| 2 | `TaskLabel::name`: `&*frame.name` | the string is alive for `'a` | `name: &str` is borrowed by the `run_labeled` call that owns the frame | Miri-SB |
| 3 | `TaskLabel::parent`: `parent.as_ref()` | the outer frame lives longer than the inner one | LIFO on one thread's stack | Miri-SB (`nested_labels_form_a_chain_and_unwind`) |

## `pragma_sys/src/windows.rs` — 6 (DECISIONS #27)

All calls are Win32 via `windows-sys`, on the `GetCurrentThread()` pseudo-handle: it does not need to be
closed and has the rights to query and change the thread's information.

| # | Call | Invariant | Who guarantees it | Checked by |
|---|---|---|---|---|
| 1 | `GetLastError` | no preconditions | — | OS test (error paths) |
| 2 | `SetThreadDescription(h, wide)` | `wide` is nul-terminated UTF-16 that lives until the end of the call | a local `Vec` with `push(0)`; length ≤ 256 + 1 (M5b) | OS test `name_is_applied` |
| 3 | `SetThreadPriority` | the value is a documented constant | a `match` on `ThreadPriority` | OS test `lower_priority_reads_back` |
| 4 | `GetThreadPriority` | an error is `THREAD_PRIORITY_ERROR_RETURN` | the return value is checked | OS test |
| 5 | `SetThreadAffinityMask` | a single-bit mask, core < 64 | the `core < MAX_CORES` check and `checked_shl` | OS tests `pinned_thread_runs_on_its_core`, `out_of_range_core_is_an_error` |
| 6 | `GetCurrentProcessorNumber` | no preconditions | — | OS test |

## `pragma_sys/src/linux.rs` — 9 (DECISIONS #27)

| # | Call | Invariant | Who guarantees it | Checked by |
|---|---|---|---|---|
| 1 | `pthread_setname_np(pthread_self(), c)` | a C string of ≤ 16 bytes including the nul | truncation to 15 bytes at a character boundary, `CString::new` | OS test (Linux CI) |
| 2 | `gettid` | no preconditions | — | OS test |
| 3 | `setpriority(PRIO_PROCESS, tid, nice)` | only the current thread's `nice` changes | `tid()` of the current thread | OS test |
| 4 | `*__errno_location() = 0` | the pointer to the current thread's errno is valid | glibc | OS test |
| 5 | `getpriority` | read-only; −1 is told apart from an error by errno | errno reset in 4 | OS test |
| 6 | `mem::zeroed::<cpu_set_t>()` | the all-zero pattern is a valid empty set | `cpu_set_t` is an array of integers | OS test |
| 7 | `CPU_SET(core, &mut set)` | the bit lies inside `set` | the `core < CPU_SETSIZE` check | OS test `out_of_range_core_is_an_error` |
| 8 | `sched_setaffinity(0, size, &set)` | the size and the pointer come from the local `set` | `size_of::<cpu_set_t>()` | OS test |
| 9 | `sched_getcpu` | no preconditions | — | OS test |

Review remark. With an invalid tid, `tid()` would return 0, and `who = 0` in `setpriority` means
"the caller". Unreachable: `gettid` is always positive. Left as is.

The Linux code is checked on a Windows machine with a cross clippy:
`cargo clippy --target x86_64-unknown-linux-gnu -p pragma_sys --lib`.

## Test allocators — `pragma_graph/tests/alloc.rs` (9, DECISIONS #19), `pragma_base/tests/oom.rs` (7, DECISIONS #37)

| Where | Invariant | Who guarantees it | Checked by |
|---|---|---|---|
| `unsafe impl GlobalAlloc` | the `GlobalAlloc` contract is upheld | all methods delegate to `System` with the same arguments; instead of allocating, `oom.rs` returns null, which the contract allows | tests |
| `unsafe fn alloc/alloc_zeroed/realloc/dealloc` | the contract is passed through from the caller | the trait signature | tests |
| `System.*` blocks | `ptr` was allocated by `System` through this same allocator | there is one global allocator per binary | tests (`tests/alloc.rs`: 0 allocations over 2000 frames; `tests/oom.rs`: `OutOfMemory` without `abort`) |

`oom.rs` does not override `alloc_zeroed`: the default implementation calls `alloc` and therefore
fails too.

## Summary

- No bugs found. Two remarks (next to `join.rs` and `linux.rs`), with no code changes.
- Every occurrence is covered by Miri, loom or a test on a real OS. Miri does not execute FFI, and
  `pragma_jobs` under Miri does not touch those paths: a pool without `with_thread_priority`/`with_affinity`
  does not call `pragma_sys`, and thread names are set by `std::thread::Builder`.
- New `unsafe` only with a DECISIONS entry and a budget update (reliability §3).
