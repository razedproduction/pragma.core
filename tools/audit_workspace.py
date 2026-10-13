#!/usr/bin/env python3
"""Аудит rust-pure workspace.

Проверяет:
  1. Чистоту: нет крейтов, компилирующих встроенный C/C++ (стоп-лист +
     build-зависимости cc/cmake/bindgen) — по `cargo metadata`.
  2. Слои: каждый внутренний крейт зависит только от крейтов строго
     нижнего слоя (layers.toml), кроме явных исключений [allow].
  3. Гигиену крейтов: `[lints] workspace = true` и `#![forbid(unsafe_code)]`
     (кроме крейтов из unsafe_allowed).

Использование:
  python3 audit_workspace.py <workspace-dir> [--no-metadata] [--all-features]
                             [--targets=x86_64-unknown-linux-gnu,wasm32-unknown-unknown]
По умолчанию: фичи по умолчанию, все платформы сразу (строже всего).
Код выхода 1 при ошибках, 0 если чисто (предупреждения не роняют).
"""
from __future__ import annotations

import json
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
    if "--no-metadata" not in sys.argv:
        accepted = layers_cfg.get("purity", {}).get("accepted", {})
        check_purity(root, set(members), r, accepted)
    print(f"Аудит {root} — крейтов: {len(members)}")
    sys.exit(r.print())


if __name__ == "__main__":
    main()
