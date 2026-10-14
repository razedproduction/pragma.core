#!/usr/bin/env python3
"""Бенч-гейт: A/B базовой ревизии и PR на одном раннере (веха M5, docs/perf.md §6).

Обе ревизии собираются заранее, затем замеры идут вперемешку: base → head → base → head …
(`--rounds` кругов). По каждому замеру берётся лучшее (минимальное) значение медианы
criterion по кругам: шум общего раннера только замедляет, поэтому минимум — самая
устойчивая оценка.

Регрессия считается так:
  ratio      = head / base по самому замеру;
  ref_ratio  = head / base по его эталону (rayon или последовательный код в том же бенче) —
               на него изменения pragma не влияют, его сдвиг — шум раннера;
  normalized = ratio / ref_ratio.
Замер с эталоном падает, если ratio > 1 + threshold И normalized > 1 + threshold.
Замер без эталона (frame_sim) падает, если ratio > 1 + threshold + noise, где noise — самый
большой сдвиг эталонов в этом прогоне.

Отдельно (предупреждение, не падение): fib(20) у pragma ≤ 2× rayon на головной ревизии.

Использование:
  python3 tools/bench_gate.py --base <worktree base> --head <worktree head>
                              [--rounds 3] [--threshold 0.10] [--summary FILE]
Код выхода 1 при регрессии, 2 при ошибке запуска.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# (пакет, бенч-таргет, фильтр criterion).
BENCHES = [
    ("pragma_jobs", "jobs", r"^(join/|spawn/(pragma|rayon)/scope_1000|parallel_for/)"),
    ("pragma_graph", "graph", r"^frame_graph_50/"),
]
CRITERION_ARGS = ["--warm-up-time", "1", "--measurement-time", "3", "--noplot"]

# Замер → эталон, на который изменения pragma не влияют.
REFERENCES = {
    "join/pragma_fib20": "join/rayon_fib20",
    "join/pragma_single": "join/rayon_single",
    "spawn/pragma_scope_1000": "spawn/rayon_scope_1000",
    "parallel_for/pragma/1000000": "parallel_for/rayon/1000000",
    "parallel_for/pragma_reduce/1000000": "parallel_for/rayon_reduce/1000000",
    "frame_graph_50/graph_16": "frame_graph_50/sequential_16",
    "frame_graph_50/graph_4096": "frame_graph_50/sequential_4096",
}
FIB = ("join/pragma_fib20", "join/rayon_fib20")
FIB_LIMIT = 2.0
FRAME_SIM_TICKS = 300


@dataclass
class Side:
    name: str
    root: Path
    target: Path

    def env(self) -> dict[str, str]:
        env = dict(os.environ)
        env["CARGO_TARGET_DIR"] = str(self.target)
        return env


def run(cmd: list[str], side: Side) -> str:
    print(f"[{side.name}] $ {' '.join(cmd)}", flush=True)
    done = subprocess.run(
        cmd, cwd=side.root, env=side.env(), capture_output=True, text=True, encoding="utf-8"
    )
    if done.returncode != 0:
        sys.stderr.write(done.stdout + done.stderr)
        raise SystemExit(2)
    return done.stdout


def criterion_results(side: Side) -> dict[str, float]:
    """Медианы (нс) последнего прогона: target/criterion/<id>/new/estimates.json."""
    base = side.target / "criterion"
    results = {}
    for path in base.glob("**/new/estimates.json"):
        bench_id = path.parent.parent.relative_to(base).as_posix()
        data = json.loads(path.read_text(encoding="utf-8"))
        results[bench_id] = float(data["median"]["point_estimate"])
    return results


DURATION = re.compile(r"(\d+) ticks in ([\d.]+)(ns|µs|us|ms|s)\b")
SCALE_MS = {"ns": 1e-6, "µs": 1e-3, "us": 1e-3, "ms": 1.0, "s": 1e3}


def frame_sim_ms_per_tick(output: str) -> float:
    """Мс/тик из строки отчёта; понимает и вывод без колонки `ms/tick` (старые ревизии)."""
    match = DURATION.search(output)
    if not match:
        sys.stderr.write(output)
        raise SystemExit(2)
    ticks, value, unit = int(match[1]), float(match[2]), match[3]
    return value * SCALE_MS[unit] / ticks


def frame_sim_workers() -> list[int]:
    cores = os.cpu_count() or 1
    return sorted({1, max(1, cores - 1)})


def build(side: Side) -> None:
    for package, bench, _ in BENCHES:
        run(["cargo", "bench", "-p", package, "--bench", bench, "--no-run"], side)
    run(["cargo", "build", "-p", "frame_sim", "--release"], side)


def measure(side: Side) -> dict[str, float]:
    results: dict[str, float] = {}
    for package, bench, pattern in BENCHES:
        run(
            ["cargo", "bench", "-p", package, "--bench", bench, "--", pattern, *CRITERION_ARGS],
            side,
        )
    results.update(criterion_results(side))
    exe = side.target / "release" / ("frame_sim.exe" if os.name == "nt" else "frame_sim")
    for workers in frame_sim_workers():
        # Два запуска подряд, лучший: время включает создание пула и шумит сильнее criterion.
        ms = min(
            frame_sim_ms_per_tick(
                run([str(exe), "--ticks", str(FRAME_SIM_TICKS), "--workers", str(workers)], side)
            )
            for _ in range(2)
        )
        # Нс, как у criterion, чтобы таблица была однородной.
        results[f"frame_sim/workers_{workers}"] = ms * 1e6
    return results


def best(rounds: list[dict[str, float]]) -> dict[str, float]:
    merged: dict[str, float] = {}
    for result in rounds:
        for key, value in result.items():
            merged[key] = min(value, merged.get(key, value))
    return merged


def fmt_ns(ns: float) -> str:
    if ns >= 1e6:
        return f"{ns / 1e6:.3f} ms"
    if ns >= 1e3:
        return f"{ns / 1e3:.2f} µs"
    return f"{ns:.1f} ns"


def main() -> int:
    # Консоль раннера Windows — cp1252: кириллица отчёта иначе роняет скрипт.
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(encoding="utf-8", errors="replace")
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--head", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--threshold", type=float, default=0.10)
    parser.add_argument("--summary", type=Path)
    args = parser.parse_args()

    head_root = args.head.resolve()
    sides = [
        Side("base", args.base.resolve(), head_root / "target" / "bench-gate-base"),
        Side("head", head_root, head_root / "target" / "bench-gate-head"),
    ]
    for side in sides:
        build(side)
    rounds: dict[str, list[dict[str, float]]] = {"base": [], "head": []}
    for r in range(args.rounds):
        for side in sides:
            print(f"--- round {r + 1}/{args.rounds}: {side.name}", flush=True)
            rounds[side.name].append(measure(side))
    base, head = best(rounds["base"]), best(rounds["head"])

    ref_ratios = {
        ref: head[ref] / base[ref] for ref in set(REFERENCES.values()) if ref in base and ref in head
    }
    noise = max((abs(r - 1.0) for r in ref_ratios.values()), default=0.0)
    limit = 1.0 + args.threshold

    rows, failures = [], []
    gated = sorted(k for k in head if k in base and (k in REFERENCES or k.startswith("frame_sim/")))
    for key in gated:
        ratio = head[key] / base[key]
        ref = REFERENCES.get(key)
        if ref in ref_ratios:
            normalized = ratio / ref_ratios[ref]
            failed = ratio > limit and normalized > limit
            note = f"{ref_ratios[ref]:.3f} ({ref.split('/', 1)[1]})"
        else:
            normalized = ratio
            failed = ratio > limit + noise
            note = f"порог {limit + noise:.3f} (шум {noise:.3f})"
        verdict = "РЕГРЕССИЯ" if failed else "ok"
        if failed:
            failures.append(key)
        rows.append(
            f"| `{key}` | {fmt_ns(base[key])} | {fmt_ns(head[key])} | {ratio:.3f} | "
            f"{note} | {normalized:.3f} | {verdict} |"
        )

    lines = [
        f"### Бенч-гейт ({os.cpu_count()} ядер, {args.rounds} кругов A/B, порог +{args.threshold:.0%})",
        "",
        "| Замер | base | head | head/base | эталон head/base | нормировано | итог |",
        "|---|---|---|---|---|---|---|",
        *rows,
        "",
        f"Шум раннера (наибольший сдвиг эталонов): {noise:.1%}.",
    ]
    fib_ok = True
    if FIB[0] in head and FIB[1] in head:
        fib_ratio = head[FIB[0]] / head[FIB[1]]
        fib_ok = fib_ratio <= FIB_LIMIT
        lines.append(
            f"fib(20): pragma / rayon = {fib_ratio:.2f}× (цель ≤ {FIB_LIMIT:.0f}×, "
            f"запас {FIB_LIMIT / fib_ratio - 1:+.0%})."
        )
    report = "\n".join(lines)
    print(report)
    if args.summary:
        with args.summary.open("a", encoding="utf-8") as fh:
            fh.write(report + "\n")
    if not fib_ok:
        print(f"::warning::fib(20) pragma/rayon выше {FIB_LIMIT}× на этом раннере")
    if failures:
        print(f"::error::регрессия > {args.threshold:.0%}: {', '.join(failures)}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
