#!/usr/bin/env python3
"""Проверка, что снимок спецификации App Store Connect API не отстал от Apple.

Использование (из корня репозитория или из любого каталога):

    python3 crates/appstore-connect-api/spec/spec_drift.py
    python3 crates/appstore-connect-api/spec/spec_drift.py --url <адрес zip или JSON>

Скрипт скачивает официальную спецификацию во временный каталог и сравнивает SHA-256
её JSON со снимком `spec/openapi.oas.json`. Если они разные, печатает отчёт
`update_spec.py --check --no-build`: добавленные, удалённые и ставшие устаревшими
операции, смену пути или метода, новые теги без набора. Снимок не меняется.

Коды выхода: 0 — снимок совпадает с Apple, 3 — есть расхождение (снимок обновляют
скриптом `update_spec.py` обычным изменением), 1 — спецификация недоступна или не
разобралась.

Только стандартная библиотека Python 3.11+. Токены и ключи не нужны: файл публичный.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import zipfile
from pathlib import Path

SPEC_DIR = Path(__file__).resolve().parent
SNAPSHOT = SPEC_DIR / "openapi.oas.json"
UPDATE_SCRIPT = SPEC_DIR / "update_spec.py"
DEFAULT_URL = (
    "https://developer.apple.com/sample-code/app-store-connect/"
    "app-store-connect-openapi-specification.zip"
)
ATTEMPTS = 3
TIMEOUT = 60
DRIFT = 3


def download(url: str, target: Path) -> None:
    """Скачивает файл с несколькими попытками: сеть раннера бывает нестабильной."""
    request = urllib.request.Request(url, headers={"User-Agent": "appstore-connect-mcp spec-drift"})
    for attempt in range(1, ATTEMPTS + 1):
        try:
            with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
                target.write_bytes(response.read())
            return
        except (urllib.error.URLError, TimeoutError, OSError) as err:
            # 4xx — адрес сменился или файл убрали: повтор не поможет.
            client_error = isinstance(err, urllib.error.HTTPError) and 400 <= err.code < 500
            if attempt == ATTEMPTS or client_error:
                sys.exit(f"не удалось скачать {url}: {err}")
            print(f"попытка {attempt} не удалась ({err}), повтор", file=sys.stderr)
            time.sleep(5 * attempt)


def spec_bytes(path: Path) -> bytes:
    """JSON спецификации: сам файл или единственный JSON из zip (без __MACOSX)."""
    if not zipfile.is_zipfile(path):
        return path.read_bytes()
    with zipfile.ZipFile(path) as archive:
        names = [
            name
            for name in archive.namelist()
            if name.endswith(".json") and not name.startswith("__MACOSX/")
        ]
        if len(names) != 1:
            sys.exit(f"в архиве ожидался один JSON, найдено: {names}")
        return archive.read(names[0])


def version(raw: bytes) -> str:
    try:
        return str(json.loads(raw).get("info", {}).get("version"))
    except json.JSONDecodeError as err:
        sys.exit(f"спецификация Apple не JSON: {err}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--url", default=DEFAULT_URL, help="адрес спецификации (zip или JSON)")
    args = parser.parse_args()

    with tempfile.TemporaryDirectory() as tmp:
        downloaded = Path(tmp) / "spec.download"
        download(args.url, downloaded)
        new_raw = spec_bytes(downloaded)
        new_version = version(new_raw)
        old_raw = SNAPSHOT.read_bytes()
        new_digest = hashlib.sha256(new_raw).hexdigest()
        old_digest = hashlib.sha256(old_raw).hexdigest()
        print(f"снимок: версия {version(old_raw)}, sha256 {old_digest}")
        print(f"Apple:  версия {new_version}, sha256 {new_digest}")
        if new_raw == old_raw:
            print("снимок совпадает со спецификацией Apple")
            return 0

        print("\nспецификация Apple отличается от снимка, отчёт update_spec.py --check:\n")
        sys.stdout.flush()
        new_file = Path(tmp) / "openapi.oas.json"
        new_file.write_bytes(new_raw)
        report = subprocess.run(
            [sys.executable, "-I", str(UPDATE_SCRIPT), "--check", "--no-build", str(new_file)],
            check=False,
        )
        if report.returncode != 0:
            print(f"update_spec.py завершился с кодом {report.returncode}", file=sys.stderr)
            return 1

    print(
        "\nЕсли операции и теги не изменились, поменялись схемы или описания: имена "
        "инструментов и ошибки генератора покажет локальный запуск update_spec.py --check "
        "(со сборкой). Снимок обновляют скриптом update_spec.py по разделу «Обновление "
        "спецификации» в docs/development.md."
    )
    return DRIFT


if __name__ == "__main__":
    sys.exit(main())
