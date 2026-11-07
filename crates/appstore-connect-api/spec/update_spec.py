#!/usr/bin/env python3
"""Обновление снимка спецификации App Store Connect API.

Использование (из корня репозитория или из любого каталога):

    python3 crates/appstore-connect-api/spec/update_spec.py <путь к openapi.oas.json или .zip>
    python3 crates/appstore-connect-api/spec/update_spec.py --check <файл>   # только отчёт

Скрипт:
1. читает новую спецификацию (JSON или zip-архив Apple с одним `*.json` внутри);
2. сравнивает её со снимком `spec/openapi.oas.json`: добавленные и удалённые операции,
   операции со сменой пути или метода, новые теги без набора в `spec/overlay.toml`;
3. записывает снимок без изменений и его SHA-256 (`spec/openapi.oas.json.sha256`);
4. собирает крейт с переменной `APPSTORE_CONNECT_SPEC_REPORT` до и после замены и
   сравнивает имена инструментов: какие имена пропали или поменялись (это ломает клиентов),
   какие появились; печатает ошибки генератора, если дополнение надо поправить.

С `--check` снимок не меняется: сравнение операций и тегов и сборка с новой спецификацией
во временной копии отчёта (снимок возвращается на место).

Только стандартная библиотека Python 3.11+ (нужен `tomllib`). Сеть не используется.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import tomllib
import zipfile
from pathlib import Path

SPEC_DIR = Path(__file__).resolve().parent
CRATE_DIR = SPEC_DIR.parent
SNAPSHOT = SPEC_DIR / "openapi.oas.json"
CHECKSUM = SPEC_DIR / "openapi.oas.json.sha256"
OVERLAY = SPEC_DIR / "overlay.toml"
REPORT_ENV = "APPSTORE_CONNECT_SPEC_REPORT"
METHODS = ("get", "post", "put", "patch", "delete")


def read_spec(path: Path) -> bytes:
    """Байты спецификации: сам файл или единственный JSON из zip-архива."""
    if zipfile.is_zipfile(path):
        with zipfile.ZipFile(path) as archive:
            names = [
                name
                for name in archive.namelist()
                if name.endswith(".json") and not name.startswith("__MACOSX/")
            ]
            if len(names) != 1:
                sys.exit(f"в архиве {path} ожидался один JSON, найдено: {names}")
            return archive.read(names[0])
    return path.read_bytes()


def operations(spec: dict) -> dict[str, tuple[str, str, str, bool]]:
    """operationId → (метод, путь, первый тег, deprecated)."""
    result = {}
    for path, item in spec.get("paths", {}).items():
        for method in METHODS:
            op = item.get(method)
            if op is None:
                continue
            tags = op.get("tags") or [""]
            result[op["operationId"]] = (
                method.upper(),
                path,
                tags[0],
                bool(op.get("deprecated")),
            )
    return result


def build_report(label: str) -> dict | None:
    """Собирает крейт и возвращает отчёт генератора (он пишется и при ошибках)."""
    with tempfile.TemporaryDirectory() as tmp:
        report = Path(tmp) / "report.json"
        env = dict(os.environ, **{REPORT_ENV: str(report)})
        proc = subprocess.run(
            ["cargo", "build", "-p", "appstore-connect-api", "--quiet"],
            cwd=CRATE_DIR,
            env=env,
            capture_output=True,
            text=True,
        )
        if not report.exists():
            print(f"[{label}] сборка не дала отчёта:\n{proc.stderr}", file=sys.stderr)
            return None
        data = json.loads(report.read_text())
        if proc.returncode != 0 and not data.get("errors"):
            print(f"[{label}] сборка упала:\n{proc.stderr}", file=sys.stderr)
        return data


def print_list(title: str, items: list[str]) -> None:
    print(f"\n{title}: {len(items)}")
    for item in items:
        print(f"  {item}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("spec", type=Path, help="новая спецификация: .json или .zip от Apple")
    parser.add_argument(
        "--check", action="store_true", help="только отчёт, снимок не менять"
    )
    parser.add_argument(
        "--no-build", action="store_true", help="не собирать крейт (без сравнения имён)"
    )
    args = parser.parse_args()

    raw = read_spec(args.spec)
    try:
        new_spec = json.loads(raw)
    except json.JSONDecodeError as err:
        sys.exit(f"{args.spec}: не JSON: {err}")
    old_raw = SNAPSHOT.read_bytes()
    old_spec = json.loads(old_raw)
    print(
        f"версия снимка: {old_spec.get('info', {}).get('version')}, "
        f"новая: {new_spec.get('info', {}).get('version')}"
    )

    old_ops, new_ops = operations(old_spec), operations(new_spec)
    print(f"операций: было {len(old_ops)}, стало {len(new_ops)}")
    print_list(
        "добавлены",
        [f"{i} {new_ops[i][0]} {new_ops[i][1]}" for i in sorted(new_ops.keys() - old_ops.keys())],
    )
    print_list(
        "удалены",
        [f"{i} {old_ops[i][0]} {old_ops[i][1]}" for i in sorted(old_ops.keys() - new_ops.keys())],
    )
    print_list(
        "сменили метод или путь",
        [
            f"{i}: {old_ops[i][0]} {old_ops[i][1]} → {new_ops[i][0]} {new_ops[i][1]}"
            for i in sorted(old_ops.keys() & new_ops.keys())
            if old_ops[i][:2] != new_ops[i][:2]
        ],
    )
    print_list(
        "стали устаревшими",
        sorted(i for i in old_ops.keys() & new_ops.keys() if new_ops[i][3] and not old_ops[i][3]),
    )

    overlay = tomllib.loads(OVERLAY.read_text())
    mapped = {tag for toolset in overlay["toolsets"] for tag in toolset["tags"]}
    new_tags = {op[2] for op in new_ops.values()}
    print_list(
        "теги без набора (добавьте в [[toolsets]] spec/overlay.toml)",
        sorted(new_tags - mapped),
    )
    print_list("теги набора, которых нет в спецификации", sorted(mapped - new_tags))

    if raw == old_raw:
        print("\nснимок не изменился")
        return 0

    before = None if args.no_build else build_report("до")
    SNAPSHOT.write_bytes(raw)
    digest = hashlib.sha256(raw).hexdigest()
    CHECKSUM.write_text(f"{digest}  {SNAPSHOT.name}\n")
    try:
        after = None if args.no_build else build_report("после")
    finally:
        if args.check:
            SNAPSHOT.write_bytes(old_raw)
            CHECKSUM.write_text(f"{hashlib.sha256(old_raw).hexdigest()}  {SNAPSHOT.name}\n")

    if before and after:
        old_tools = {t["operation_id"]: t["tool"] for t in before["tools"]}
        new_tools = {t["operation_id"]: t["tool"] for t in after["tools"]}
        print_list(
            "имена инструментов сменились (ломает клиентов)",
            [
                f"{old_tools[i]} → {new_tools[i]} ({i})"
                for i in sorted(old_tools.keys() & new_tools.keys())
                if old_tools[i] != new_tools[i]
            ],
        )
        print_list(
            "инструменты пропали (ломает клиентов)",
            sorted(f"{old_tools[i]} ({i})" for i in old_tools.keys() - new_tools.keys()),
        )
        print_list(
            "новые инструменты",
            sorted(f"{new_tools[i]} ({i})" for i in new_tools.keys() - old_tools.keys()),
        )
        print(
            f"\nинструментов: было {before['tools_total']}, стало {after['tools_total']}; "
            f"исключено: было {before['excluded_total']}, стало {after['excluded_total']}"
        )
    if after and after.get("errors"):
        print_list("ошибки генератора (поправьте spec/overlay.toml)", after["errors"])
        status = 1
    else:
        status = 0

    if args.check:
        print("\n--check: снимок не изменён")
    else:
        print(f"\nзаписан {SNAPSHOT.relative_to(CRATE_DIR)} (sha256 {digest})")
        print(
            "дальше: cargo test -p appstore-connect-api; "
            "изменения снапшотов — INSTA_UPDATE=always cargo test -p appstore-connect-api"
        )
    return status


if __name__ == "__main__":
    sys.exit(main())
