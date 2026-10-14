#!/usr/bin/env python3
"""Аудит rust-pure workspace.

Проверяет:
  1. Чистоту: нет крейтов, компилирующих встроенный C/C++ (стоп-лист +
     build-зависимости cc/cmake/bindgen) — по `cargo metadata`.
  2. Слои: каждый внутренний крейт зависит только от крейтов строго
     нижнего слоя (layers.toml), кроме явных исключений [allow].
  3. Гигиену крейтов: `[lints] workspace = true` и `#![forbid(unsafe_code)]`
     (кроме крейтов из unsafe_allowed).
  4. Счётчик `unsafe` по файлам против `[unsafe_budget]` в layers.toml; с
     `--unsafe-baseline=<layers.toml базовой ревизии>` рост требует новой записи DECISIONS.md.

Использование:
  python3 audit_workspace.py <workspace-dir> [--no-metadata] [--all-features]
                             [--targets=x86_64-unknown-linux-gnu,wasm32-unknown-unknown]
                             [--unsafe-baseline=<file>]
По умолчанию: фичи по умолчанию, все платформы сразу (строже всего).
Код выхода 1 при ошибках, 0 если чисто (предупреждения не роняют).
"""
from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    sys.exit("Нужен Python 3.11+ (tomllib)")

# Крейты, которые компилируют встроенный C/C++ (или C/asm) код.
VENDORED_C = {
    "cc": "компилятор C/C++ в build-скрипте",
    "cmake": "сборка C/C++ через CMake",
    "bindgen": "генерация привязок к C-заголовкам",
    "zstd-sys": "используйте ruzstd",
    "libz-sys": "используйте miniz_oxide",
    "lz4-sys": "используйте lz4_flex",
    "ring": "C/asm; для TLS — rustls-rustcrypto",
    "aws-lc-sys": "C",
    "aws-lc-fips-sys": "C",
    "openssl-sys": "C (OpenSSL); используйте rustls",
    "shaderc-sys": "используйте naga",
    "glslang-sys": "используйте naga",
    "spirv-cross-sys": "используйте naga",
    "sdl2-sys": "используйте winit",
    "sdl3-sys": "используйте winit",
    "glfw-sys": "используйте winit",
    "physx-sys": "используйте rapier",
    "mlua-sys": "встроенный Lua; используйте rhai/wasmi",
    "lua-src": "встроенный Lua",
    "luajit-src": "встроенный LuaJIT",
    "tracy-client-sys": "C++; используйте puffin",
    "freetype-sys": "используйте swash/fontdue",
    "harfbuzz-sys": "используйте rustybuzz",
    "imgui-sys": "C++; используйте egui",
    "libwebp-sys": "C",
    "dav1d-sys": "C",
    "openal-sys": "используйте cpal/kira",
    "assimp-sys": "используйте gltf",
    "russimp-sys": "используйте gltf",
    "basis-universal-sys": "C++",
    "libsqlite3-sys": "C (SQLite)",
}
# Привязки к системным библиотекам ОС — допустимы, но требуют dev-пакетов.
SYSTEM_FFI = {
    "alsa-sys": "системная libasound (apt: libasound2-dev)",
    "libudev-sys": "системная libudev (apt: libudev-dev)",
    "pkg-config": "поиск системных библиотек",
}
# Инструменты вне дерева движка (fuzz и т.п.), которые можно игнорировать.
DEV_TOOL_OK = {"libfuzzer-sys"}


class Report:
    def __init__(self) -> None:
        self.errors: list[str] = []
        self.warnings: list[str] = []
        self.info: list[str] = []

    def print(self) -> int:
        for title, items, mark in (("ОШИБКИ", self.errors, "✗"),
                                   ("Предупреждения", self.warnings, "!"),
                                   ("Инфо", self.info, "i")):
            if items:
                print(f"\n{title}:")
                for it in items:
                    print(f"  {mark} {it}")
        if not self.errors:
            print("\n✓ Аудит пройден" + (" (есть предупреждения)" if self.warnings else ""))
            return 0
        print(f"\n✗ Ошибок: {len(self.errors)}")
        return 1


def load_toml(p: Path) -> dict:
    with p.open("rb") as f:
        return tomllib.load(f)


def workspace_members(root: Path) -> dict[str, Path]:
    ws = load_toml(root / "Cargo.toml").get("workspace", {})
    members: dict[str, Path] = {}
    for pattern in ws.get("members", []):
        for d in sorted(root.glob(pattern)):
            ct = d / "Cargo.toml"
            if ct.is_file():
                name = load_toml(ct).get("package", {}).get("name")
                if name:
                    members[name] = d
    return members


def internal_deps(manifest: dict, members: set[str]) -> set[str]:
    out: set[str] = set()
    sections = [manifest.get("dependencies", {}), manifest.get("build-dependencies", {})]
    for t in manifest.get("target", {}).values():
        sections += [t.get("dependencies", {}), t.get("build-dependencies", {})]
    for sec in sections:
        for key, val in sec.items():
            real = val.get("package", key) if isinstance(val, dict) else key
            if real in members:
                out.add(real)
    return out


def check_layers(root: Path, members: dict[str, Path], r: Report) -> dict:
    lp = root / "layers.toml"
    if not lp.is_file():
        r.warnings.append("layers.toml не найден — проверка слоёв пропущена")
        return {}
    cfg = load_toml(lp)
    layers: dict[str, int] = cfg.get("crates", {})
    allow: dict[str, list[str]] = cfg.get("allow", {})
    for name in members:
        if name not in layers:
            r.errors.append(f"{name}: нет в layers.toml — назначьте слой")
    for name, d in members.items():
        if name not in layers:
            continue
        manifest = load_toml(d / "Cargo.toml")
        for dep in sorted(internal_deps(manifest, set(members))):
            if dep not in layers:
                continue
            if layers[dep] < layers[name] or dep in allow.get(name, []):
                continue
            r.errors.append(
                f"{name} (L{layers[name]}) зависит от {dep} (L{layers[dep]}): "
                "зависимость вверх или вбок по слою. Перенесите код ниже или добавьте "
                "исключение в [allow] с записью в DECISIONS.md")
    return cfg


def check_hygiene(members: dict[str, Path], layers_cfg: dict, r: Report) -> None:
    unsafe_ok = set(layers_cfg.get("unsafe_allowed", []))
    for name, d in members.items():
        manifest = load_toml(d / "Cargo.toml")
        if not manifest.get("lints", {}).get("workspace"):
            r.errors.append(f"{name}: нет `[lints] workspace = true` — строгие линты не применяются")
        roots = [p for p in (d / "src" / "lib.rs", d / "src" / "main.rs") if p.is_file()]
        for rp in roots:
            text = rp.read_text(encoding="utf-8", errors="replace")
            has_forbid = "#![forbid(unsafe_code)]" in text
            if name in unsafe_ok:
                if "undocumented_unsafe_blocks" not in text and "#![deny(unsafe_op_in_unsafe_fn)]" not in text:
                    r.warnings.append(f"{name}: unsafe разрешён — добавьте #![deny(unsafe_op_in_unsafe_fn)] "
                                      "и гоняйте тесты под Miri")
            elif not has_forbid:
                r.errors.append(f"{name}: {rp.relative_to(d)} без #![forbid(unsafe_code)] "
                                "(или добавьте крейт в unsafe_allowed в layers.toml)")


def check_purity(root: Path, members: set[str], r: Report, accepted: dict[str, str]) -> None:
    try:
        proc = subprocess.run(
            ["cargo", "metadata", "--format-version", "1"]
            + (["--all-features"] if ALL_FEATURES else [])
            + [f"--filter-platform={t}" for t in TARGETS],
            cwd=root, capture_output=True, text=True, timeout=600)
    except FileNotFoundError:
        r.warnings.append("cargo не найден — проверка чистоты пропущена")
        return
    except subprocess.TimeoutExpired:
        r.warnings.append("cargo metadata: таймаут — проверка чистоты пропущена")
        return
    if proc.returncode != 0:
        tail = proc.stderr.strip().splitlines()[-3:]
        r.warnings.append("cargo metadata не выполнился (нет сети?) — проверка чистоты пропущена: "
                          + " | ".join(tail))
        return
    meta = json.loads(proc.stdout)
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in (meta.get("resolve") or {}).get("nodes", [])}

    # обратный граф для цепочек «кто тянет»
    parents: dict[str, set[str]] = {}
    for nid, n in nodes.items():
        for d in n.get("deps", []):
            parents.setdefault(d["pkg"], set()).add(nid)

    def chain(pid: str) -> str:
        path, cur, seen = [pkgs[pid]["name"]], pid, {pid}
        while True:
            ps = [p for p in parents.get(cur, ()) if p not in seen]
            if not ps:
                break
            ws = [p for p in ps if pkgs[p]["name"] in members]
            cur = (ws or sorted(ps))[0]
            seen.add(cur)
            path.append(pkgs[cur]["name"])
            if pkgs[cur]["name"] in members:
                break
        return " ← ".join(path)

    found_c, found_sys = [], []
    c_tools = {"cc", "cmake", "bindgen"}
    for pid in nodes:
        p = pkgs[pid]
        name = p["name"]
        if name in members or name in DEV_TOOL_OK or name in c_tools:
            continue
        # реально активированные build-зависимости из resolve-графа
        bdeps = {pkgs[d["pkg"]]["name"] for d in nodes[pid].get("deps", [])
                 if any(k.get("kind") == "build" for k in d.get("dep_kinds", []))}
        uses_c = sorted(bdeps & c_tools)
        if name in VENDORED_C:
            found_c.append(f"{name} {p['version']}: {VENDORED_C[name]} — цепочка: {chain(pid)}")
        elif uses_c:
            found_c.append(f"{name} {p['version']}: build.rs вызывает {', '.join(uses_c)} "
                           f"— цепочка: {chain(pid)}")
        elif name.endswith("-src"):
            found_c.append(f"{name} {p['version']}: похоже на vendored-исходники C/C++ — цепочка: {chain(pid)}")
        elif name in SYSTEM_FFI:
            found_sys.append(f"{name}: {SYSTEM_FFI[name]} — цепочка: {chain(pid)}")
        elif name.endswith("-sys"):
            found_sys.append(f"{name}: FFI-привязка без сборки C (проверьте, что это системная "
                             f"библиотека) — цепочка: {chain(pid)}")
    def dedupe(xs):
        seen, out = set(), []
        for x in sorted(xs):
            key = x.split(" ", 1)[0].rstrip(":")
            if key not in seen:
                seen.add(key)
                out.append(x)
        return out
    found_c, found_sys = dedupe(found_c), dedupe(found_sys)
    for x in found_c:
        crate = x.split(" ", 1)[0]
        if crate in accepted:
            r.warnings.append(f"принятая C-прослойка: {x} (причина: {accepted[crate]})")
        else:
            r.errors.append(f"встроенный C/C++: {x}")
    found_c = []
    r.errors += [f"встроенный C/C++: {x}" for x in found_c]
    r.info += [f"системная FFI: {x}" for x in found_sys]
    scope = ", ".join(TARGETS) if TARGETS else "все платформы"
    feats = "все фичи" if ALL_FEATURES else "фичи по умолчанию"
    r.info.append(f"проверено пакетов в графе: {len(nodes)} ({scope}; {feats})")


def strip_comments_and_strings(src: str) -> str:
    """Код без комментариев, строковых и символьных литералов (для поиска ключевых слов)."""
    out, i, n = [], 0, len(src)
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            i = n if j < 0 else j
        elif src.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if src.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif src.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
        elif c == "r" and (src.startswith('r"', i) or src.startswith('r#', i)) and \
                (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            j = i + 1
            while j < n and src[j] == "#":
                j += 1
            if j < n and src[j] == '"':
                end = src.find('"' + "#" * (j - i - 1), j + 1)
                i = n if end < 0 else end + 1 + (j - i - 1)
            else:
                out.append(c)
                i += 1
        elif c == '"':
            i += 1
            while i < n and src[i] != '"':
                i += 2 if src[i] == "\\" else 1
            i += 1
        elif c == "'":
            # Символьный литерал ('x', '\n', '\u{..}') или лайфтайм ('a) — лайфтайм оставляем.
            if src.startswith("\\", i + 1):
                end = src.find("'", i + 2)
                i = n if end < 0 else end + 1
            elif i + 2 < n and src[i + 2] == "'":
                i += 3
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


UNSAFE_WORD = re.compile(r"\bunsafe\b")


def count_unsafe(root: Path, members: dict[str, Path]) -> dict[str, int]:
    """Вхождения ключевого слова `unsafe` в коде (блоки, `unsafe fn`/`impl`, типы
    `unsafe fn(..)`) по файлам всех крейтов workspace и `fuzz/`."""
    dirs = list(members.values()) + ([root / "fuzz"] if (root / "fuzz").is_dir() else [])
    counts: dict[str, int] = {}
    for d in dirs:
        for path in sorted(d.rglob("*.rs")):
            rel = path.relative_to(root)
            if "target" in rel.parts:
                continue
            code = strip_comments_and_strings(path.read_text(encoding="utf-8", errors="replace"))
            hits = len(UNSAFE_WORD.findall(code))
            if hits:
                counts[rel.as_posix()] = hits
    return counts


def check_unsafe_budget(root: Path, members: dict[str, Path], layers_cfg: dict,
                        baseline_cfg: dict | None, r: Report) -> None:
    """Счётчик `unsafe` против `[unsafe_budget]` в layers.toml (docs/reliability.md §3).

    Бюджет точный: рост и уменьшение без правки бюджета — ошибка. Каждая запись ссылается
    на решения DECISIONS.md; если задан baseline (layers.toml базовой ревизии), рост числа
    в файле требует решения, которого у этого файла в baseline не было.
    """
    budget: dict[str, dict] = layers_cfg.get("unsafe_budget", {})
    counts = count_unsafe(root, members)
    decisions_text = (root / "DECISIONS.md").read_text(encoding="utf-8") \
        if (root / "DECISIONS.md").is_file() else ""
    known = {int(m) for m in re.findall(r"^\|\s*(\d+)\s*\|", decisions_text, re.M)}
    base_budget: dict[str, dict] = (baseline_cfg or {}).get("unsafe_budget", {})
    unsafe_ok = set(layers_cfg.get("unsafe_allowed", []))
    for rel in sorted(set(counts) | set(budget)):
        have = counts.get(rel, 0)
        entry = budget.get(rel)
        if entry is None:
            r.errors.append(f"unsafe: {rel} — {have} вхождений без бюджета. Добавьте запись в "
                            "DECISIONS.md и строку в [unsafe_budget] layers.toml")
            continue
        want = int(entry.get("count", 0))
        refs = [int(x) for x in entry.get("decisions", [])]
        if not refs:
            r.errors.append(f"unsafe: {rel} — в бюджете нет ссылки на DECISIONS.md")
        for ref in refs:
            if ref not in known:
                r.errors.append(f"unsafe: {rel} ссылается на DECISIONS #{ref}, такой записи нет")
        if have > want:
            r.errors.append(f"unsafe: {rel} — {have} вхождений, бюджет {want}. Рост — только с "
                            "новой записью в DECISIONS.md и правкой [unsafe_budget]")
        elif have < want:
            r.errors.append(f"unsafe: {rel} — {have} вхождений, бюджет {want}: уменьшите бюджет")
        crate = next((n for n, d in members.items()
                      if (root / rel).resolve().is_relative_to(d.resolve())), None)
        is_test = any(p in ("tests", "benches") for p in Path(rel).parts)
        if crate and crate not in unsafe_ok and not is_test and have:
            r.errors.append(f"unsafe: {rel} — крейт {crate} не в unsafe_allowed")
        if baseline_cfg is not None:
            old = base_budget.get(rel, {})
            if want > int(old.get("count", 0)):
                new_refs = set(refs) - {int(x) for x in old.get("decisions", [])}
                if not new_refs:
                    r.errors.append(
                        f"unsafe: {rel} — бюджет вырос {old.get('count', 0)} → {want} без новой "
                        "записи DECISIONS.md в `decisions`")
    total = sum(counts.values())
    r.info.append(f"unsafe: {total} вхождений в {len(counts)} файлах (бюджет layers.toml)")


ALL_FEATURES = False
TARGETS: list[str] = []


def main() -> None:
    global ALL_FEATURES, TARGETS
    ALL_FEATURES = "--all-features" in sys.argv
    for a in sys.argv[1:]:
        if a.startswith("--targets="):
            TARGETS = [t for t in a.split("=", 1)[1].split(",") if t]
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    root = Path(args[0] if args else ".").resolve()
    if not (root / "Cargo.toml").is_file():
        sys.exit(f"{root}: нет Cargo.toml")
    r = Report()
    members = workspace_members(root)
    if not members:
        r.errors.append("workspace без членов")
    layers_cfg = check_layers(root, members, r)
    check_hygiene(members, layers_cfg, r)
    baseline_cfg = None
    for a in sys.argv[1:]:
        if a.startswith("--unsafe-baseline="):
            baseline_path = Path(a.split("=", 1)[1])
            baseline_cfg = load_toml(baseline_path) if baseline_path.stat().st_size else {}
    check_unsafe_budget(root, members, layers_cfg, baseline_cfg, r)
    if "--no-metadata" not in sys.argv:
        accepted = layers_cfg.get("purity", {}).get("accepted", {})
        check_purity(root, set(members), r, accepted)
    print(f"Аудит {root} — крейтов: {len(members)}")
    sys.exit(r.print())


if __name__ == "__main__":
    main()
