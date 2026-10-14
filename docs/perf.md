# Performance: methodology and results

How to measure pragma.core's performance so that the numbers can be trusted, and what has been measured.
Since M5 the same methodology is automated by the bench gate in CI (§6); manual measurements are for
tuning decisions and for the tables below.

## 1. Hardware and environment

| | |
|---|---|
| CPU | AMD FX-8350, 4 modules / **8 logical cores** (`available_parallelism() = 8`) |
| RAM | 18 GB |
| OS | Windows 10 Home, the system allocator (~90 ns per allocation) |
| Toolchain | Rust 1.99.0 (`rust-toolchain.toml`), the `bench` profile = `release` + `debug = true`, `lto = "thin"`, `codegen-units = 1` |

In the roadmap tables for M1–M3 the machine was misstated as "12 threads"; it is the same machine
with 8 logical cores. So `frame_sim --workers 11` (and `--verify 1,4,11`) is an
oversubscribed mode: 11 workers plus the main thread on 8 cores.

## 2. Commands

```bash
cargo bench -p pragma_jobs            # join, fib(20), scope, spawn, parallel_for/reduce vs rayon
cargo bench -p pragma_graph           # a 50-task graph vs sequential execution
cargo bench -p pragma_async           # rescheduling an async task and the cost of the Box in it
cargo run -p frame_sim --release -- --ticks 300 --verify 1,4,11   # ms/tick = time / 300 (latency=0ms)
```

The pools in the `pragma_jobs` benches are the same size as rayon's: `with_workers(available_parallelism())`.
In pragma the main thread also takes part in the work (an external slot); in rayon it waits in `install`.

## 3. Measurement rules

1. **Interleaved A/B.** The machine "floats" between sessions by tens of percent (heat, background
   processes): `frame_sim` on 11 workers gave 530 and 660–720 ms on the same code on the same
   day. Only runs made back to back under the same conditions are compared: branch → `main` → branch →
   `main` (`git stash -u` / `git stash pop`), at least **2 rounds**.
2. **The machine is idle during measurement.** No builds, tests or Miri in parallel.
3. **Variance.** Criterion gives an interval over 100 samples; between runs we look at the
   *reference*: rayon in the same runs, with no code changes. If rayon shifted by X%, a
   pragma change smaller than X% is noise. In the M4 measurements rayon shifted by up to ±8% (fib(20) 99–108 µs,
   reduction 230–282 µs).
4. **Determinism is checked on every run** (`frame_sim --verify`): a performance change that
   broke the hash is not accepted.
5. Criterion's `change:` column is not used: it compares with the last run of
   *any* code, not with the baseline.

## 4. M4 results (A/B, 2 rounds, `[min … max]` over two runs)

| Measurement | `main` (M4a, 7955ff0) | M4b branch | rayon |
|---|---|---|---|
| fib(20), nested `join` | 490.7 … 504.0 µs | **196.6 … 199.7 µs** | 99.0 … 108.0 µs |
| single `join` | 526 … 570 ns | **284 … 288 ns** | 1.47 … 1.51 µs |
| `scope` of 1000 tasks | 914 … 949 µs | 938 … 947 µs | 854 … 892 µs |
| `spawn` + handle `join` | 1.38 … 1.47 µs | 1.54 … 1.55 µs | — |
| `parallel_for` 10⁶ | 460 … 462 µs | 454 … 478 µs | 477 … 508 µs |
| `parallel_reduce` 10⁶ | 209 … 225 µs | 198 … 214 µs | 230 … 282 µs |
| 50-task graph, near-empty | 8.63 … 9.55 µs | 8.30 … 8.38 µs | — |
| 50-task graph, 4096 elements | 279 … 283 µs | 279 … 283 µs | — |
| `frame_sim` 1 worker, ms/tick | 5.07 … 5.23 | 5.07 … 5.37 | — |
| `frame_sim` 4 workers | 2.33 … 2.45 | 2.32 | — |
| `frame_sim` 11 workers | 1.87 | 1.83 … 1.90 | — |

- **Nested `join` ≤ 2× rayon: achieved**, 1.95–1.99× (was 4.6–5.5×). The reason for the speedup
  and the design are in DECISIONS #30: the second branch lives on the stack with a latch without `Arc`, and a branch that was not stolen
  the owner runs itself without atomic RMW.
- `spawn` + handle `join`: +5–10%, within this bench's spread between runs
  (the `idle_spin` sweep below: 1.30–1.61 µs on the same code). The `spawn` path did not change in M4b.
- Everything else is unchanged beyond noise; the `frame_sim` hash matched in all runs.

## 4a. After M5: `join` without the wakeup fence (DECISIONS #40)

Interleaved A/B, 3 rounds (`[min … max]`), FX-8350, 8 workers:

| Measurement | `main` (27186e7) | branch | rayon |
|---|---|---|---|
| fib(20), nested `join` | 200.5 … 206.5 µs | **180.7 … 184.2 µs** | 101.3 … 108.7 µs |
| single `join` | 284.6 … 297.5 ns | **250.5 … 254.7 ns** | 1.49 … 1.59 µs |

fib(20) is ~1.8× rayon (was ~2×, with ~1% headroom to the target). The 50-task graph, `scope` 1000,
`parallel_for`/`parallel_reduce` and `frame_sim` on 1/4/7 workers are unchanged beyond noise,
and the `frame_sim` hash matched in all runs.

## 5. What we tried and rejected

| Experiment (fib(20)) | Result | Conclusion |
|---|---|---|
| Remove `fence(SeqCst)` in `notify_one` (incorrect, measurement only) | 207 → 184 µs | The fence is ~2 ns per `join`; the sleep protocol loses wakeups without it, so it was kept. **Revisited after M5** (DECISIONS #40): the fence is removed for pushing the second `join` branch, see §4a |
| Never wake workers at all (measurement only) | 228–269 µs | Slower: fib(20) is bound by work distribution, not by the fast path |
| `cores − 1` workers instead of `cores` | 249–303 µs | Slower: less parallelism |
| `is_empty` before `steal` | 207 → 197–200 µs | Accepted: empty victims do not enter the crossbeam epoch |

### `idle_spin` sweep (DECISIONS #32)

One run per value (`--measurement-time 3`), identical conditions:

| `idle_spin` | fib(20) | `scope` 1000 | `spawn`+`join` | `parallel_for` | `frame_sim` 4 / 11, ms per 300 ticks |
|---|---|---|---|---|---|
| 0 | 203 µs | 950 µs | 1.47 µs | 461 µs | 785 / 637 |
| 8 | 190 µs | 1037 µs | 1.30 µs | 464 µs | 873 / 665 |
| **32** (default) | 203 µs | 906 µs | 1.33 µs | 451 µs | 783 / 669 |
| 128 | 203 µs | 964 µs | 1.61 µs | 446 µs | 785 / 710 |

The difference on fib(20) is within noise; 32 is better on `scope`, 128 worsens `spawn` and the
oversubscribed `frame_sim` (yielding workers get in the main thread's way). 32 is in the same ballpark as rayon's;
the setting is open to the application (`PoolConfig::with_idle_spin`).

### `Box` per async scheduling (DECISIONS #31)

`cargo bench -p pragma_async`: rescheduling a task (`yield_now` → pool → `poll`) is
**1.39 µs**, a `Box<dyn FnOnce>` (allocation + call + deallocation) is **88 ns**, ~6%. The main cost is the
cross-thread wakeup through the global background queue; the `Box` is kept.

## 6. Bench gate in CI

`tools/bench_gate.py`, the `bench gate` job in every PR on Windows and Linux (DECISIONS #34). It is
§3, automated:

1. The base revision (the PR base, i.e. `main`) goes into a `git worktree`. Both revisions are built into
   different target directories before the first measurement.
2. Interleaved measurements: base → head, 3 rounds. Each round is criterion (`--warm-up-time 1
   --measurement-time 3`) over `join`, fib(20), `scope` 1000, `parallel_for`/`parallel_reduce`
   10⁶, the 50-task graph, and two `frame_sim --ticks 300` runs on 1 and N = cores − 1 workers.
   For each measurement the best value across rounds is taken: shared-runner noise only slows things down.
3. Regression: `ratio = head / base`. For measurements with a reference in the same bench (rayon,
   sequential code), `ref_ratio` is the reference's shift, which pragma does not affect. It fails
   if `ratio > 1.10` **and** `ratio / ref_ratio > 1.10`. `frame_sim` has no reference and fails
   at `ratio > 1.10 + noise`, where noise is its own: 3 runs per round, median / minimum − 1
   over all runs of each revision, the larger of the two (DECISIONS #41; previously the largest
   `|ref_ratio − 1|`, i.e. the noise of the nanosecond `rayon_single`).
4. fib(20) ≤ 2× rayon on the PR head is a line in the summary and a `::warning::`, not a failure:
   the ratio depends on the core count.

The result is a table in the job summary.

### GitHub runner noise (PR #8: ubuntu ran M5a vs `main`, windows ran M5a+M5b vs M5a; the bench code did not change)

| | ubuntu-latest | windows-latest |
|---|---|---|
| Cores (`os.cpu_count()`) | 2 | 2 |
| Largest reference shift | 21.9% (`rayon_single`, 124 ns) | 35.4% (`rayon_single`, ~145 ns) |
| Shift of the other references | ≤ 0.1% | ≤ 0.3% |
| `frame_sim` 1 worker, head / base on the same code | 1.152 (one run per round) | 0.981 (best of two per round) |
| fib(20) pragma / rayon | 2.32× | 2.48× (1.98× locally on 8 cores) |

Conclusions: microsecond-scale criterion measurements are stable on both runners (±0.3%), while
the nanosecond single `join` is 20–35% noisy and `frame_sim` (time including pool creation, 3 threads on
2 cores) up to 15%. That is why `join/pragma_single` is gated only with normalization by rayon, and the
`frame_sim` threshold is inflated by the `rayon_single` noise (1.32 on ubuntu, 1.45 on windows): the gate on a 2-core runner
will not catch a `frame_sim` regression smaller than ~30–45%. This is a deliberate trade-off against false failures. Precise
measurements are still done manually per §3.

**Update (DECISIONS #41):** `frame_sim` noise is now estimated from its own runs rather than from
`rayon_single`, which has nothing to do with `frame_sim`. The threshold follows the actual spread of
`frame_sim` on the specific runner rather than the worst nanosecond bench.
