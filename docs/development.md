# Разработка

## Структура

| Крейт | Назначение |
| --- | --- |
| `crates/appstore-connect-api` | Снимок OpenAPI-спецификации Apple и файл-дополнение (`spec/`), генератор таблицы операций (`build.rs`, `build/`), исполнитель запросов (`executor`), клиент временных адресов API (`external`), выпуск JWT (`jwt`), ключи (`key`), хранилище ключей (`auth`), проверка путей (`path`). |
| `crates/appstore-connect-mcp-core` | MCP-инструменты: реестр, уровни доступа, наборы и динамические наборы, `api_search`, `api_request`, `jq`, `flatten`, вырезание секретов, политика локальных файлов, пользовательские инструменты, составные инструменты (`src/composite/`), промпты. Не зависит от транспорта. |
| `crates/appstore-connect-mcp` | Бинарник: CLI (`auth`, `connect`, `key exec`, `serve`, `toolsets`, `tools`), регистрация в клиентах (`src/clients/`) и stdio-транспорт. |

## Сборка и проверки

Нужен Rust 1.88 или новее; `rust-toolchain.toml` выбирает стабильный канал с `rustfmt` и `clippy`. Для криптографии TLS нужен C-компилятор; системные библиотеки D-Bus и OpenSSL не нужны. Криптография ключей и сертификатов (`p256`, `rsa`, PKCS#12) — на чистом Rust.

```bash
cargo build --release
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Тесты не обращаются к реальному App Store Connect API: HTTP подменяется `wiremock` (в том числе «хост загрузки» для `asset_upload` и `build_upload`), ключи ES256 и RSA генерируются в тесте, вместо системного хранилища — реализация в памяти, вместо терминала и `claude` — подставные объекты. Реальных ключей, Key ID, Issuer ID и ответов кабинетов в тестах нет. Тесты `key exec` выполняются только на Unix.

Запустить CLI из исходников:

```bash
cargo run -p appstore-connect-mcp -- --help
cargo run -p appstore-connect-mcp -- serve --help
cargo run -p appstore-connect-mcp -- toolsets
```

## Спецификация и генерация инструментов

Инструменты операций генерируются при сборке из двух файлов в `crates/appstore-connect-api/spec/`:

- `openapi.oas.json` — спецификация App Store Connect API от Apple как есть, рядом её SHA-256 (`openapi.oas.json.sha256`). `build.rs` сверяет хеш и падает, если снимок правили руками.
- `overlay.toml` — файл-дополнение: всё, чего нет в спецификации. Наборы инструментов (каждый тег спецификации входит ровно в один набор), исключения уровней доступа с причиной, сокращения и явные имена инструментов, исключённые операции.

`build.rs` превращает их в таблицу операций: имя инструмента, метод, путь, уровень доступа, набор и JSON Schema аргументов с раскрытыми `$ref`. Генерация детерминирована и идёт без сети. Ошибки сборки: тег без набора, ссылка на несуществующий `operationId`, неизвестное поле дополнения, повтор имени инструмента, имя длиннее 40 символов, исключение уровня доступа, совпадающее с уровнем по умолчанию. Сгенерированный код не правится вручную.

Отчёт генератора — число инструментов по уровням и наборам, исключённые операции, исключения доступа, самые большие схемы:

```bash
APPSTORE_CONNECT_SPEC_REPORT=/tmp/asc-spec-report.json cargo build -p appstore-connect-api
```

### Обновление спецификации

Проверить, не выпустила ли Apple новую спецификацию:

```bash
python3 crates/appstore-connect-api/spec/spec_drift.py
```

Скрипт скачивает официальный zip с developer.apple.com во временный каталог и сравнивает SHA-256 со снимком. Код 0 — снимок актуален, 3 — есть расхождение (скрипт печатает добавленные, удалённые и ставшие устаревшими операции и новые теги без набора), 1 — спецификация недоступна. Снимок он не меняет.

1. Скачайте спецификацию App Store Connect API с developer.apple.com (zip или JSON): сборка и `update_spec.py` сеть не используют.
2. Посмотрите отчёт без изменений: добавленные, удалённые и ставшие устаревшими операции, смену пути или метода, теги без набора, пропавшие и переименованные инструменты (это ломает клиентов), ошибки генератора.

   ```bash
   python3 crates/appstore-connect-api/spec/update_spec.py --check <файл>
   ```

3. Поправьте `overlay.toml`: новые теги — в наборы, длинные или конфликтующие имена — в явные имена (так же можно сохранить прежнее имя инструмента), уровни доступа новых операций с последствиями для покупателей — в исключения.
4. Запишите снимок и SHA-256 (скрипт печатает тот же отчёт):

   ```bash
   python3 crates/appstore-connect-api/spec/update_spec.py <файл>
   ```

5. `cargo test -p appstore-connect-api`. Снапшоты операций (`crates/appstore-connect-api/tests/snapshots/`) обновляются осознанно, после просмотра изменений: `cargo insta review` или `INSTA_UPDATE=always cargo test -p appstore-connect-api`. Если состав операций изменился, обновите ожидаемые числа в `tests/operations.rs` и таблицу наборов в [MCP-инструментах](tools.md#наборы-инструментов).

Составные инструменты сверяют свои таблицы со снимком тестами: например, список ресурсов `asset_upload` и перечисления `report_download`. После обновления спецификации эти тесты подскажут, что добавить. Тест `tests/tool_names.rs` крейта `appstore-connect-mcp-core` проверяет, что имена инструментов в промптах и подсказках существуют.

## Статическая сборка под Linux

Релизные Linux-бинарники статические (musl): без `PT_INTERP` и `DT_NEEDED`, без зависимости от glibc, `libdbus` и OpenSSL. Secret Service работает через `zbus` на чистом Rust.

```bash
scripts/release/musl-build.sh x86_64 out/     # или aarch64
```

Скрипт запускается на Linux (например, в контейнере `rust:<версия>-bookworm`): ставит тулчейн и target через `rustup`, Zig и `cargo-zigbuild`; Zig служит C-компилятором и линкером, поэтому любую из двух архитектур можно собрать на любом Linux-хосте. Нужен `readelf` (binutils). После сборки он проверяет архитектуру ELF и отсутствие динамических зависимостей и кладёт бинарник в `out/appstore-connect-mcp`.

Проверка бинарника в «голом» образе (Alpine, Debian slim):

```bash
scripts/ci/musl-smoke.sh out/appstore-connect-mcp                         # без Secret Service: запуск, справка, понятные ошибки без паники
SMOKE_KEY_FILE=test-key.p8 scripts/ci/musl-smoke.sh out/appstore-connect-mcp --with-secret-service
```

С `--with-secret-service` скрипт (под root, через `apk` или `apt-get`) ставит D-Bus, gnome-keyring и libsecret, поднимает сессионную шину с разблокированным хранилищем и проверяет настоящий путь ключа: `auth login --no-verify`, `auth status`, MCP `initialize` и `tools/list` через stdio, `auth logout`. Тестовый ключ — сгенерированный EC P-256 в PKCS#8 PEM (например, `openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out test-key.p8`), Key ID и Issuer ID фиктивные; к App Store Connect API скрипт не обращается.

Скрипты оболочки проверяются `bash -n` (или `sh -n` для POSIX) и `shellcheck`.

## Windows

Сборка — нативная, целевая платформа `x86_64-pc-windows-msvc`: нужны Visual Studio Build Tools с компонентом «Разработка классических приложений на C++» и `rustup`. Релизный бинарник собирается со статическим CRT, чтобы на машине пользователя не требовался Visual C++ Redistributable:

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked --target x86_64-pc-windows-msvc -p appstore-connect-mcp
cargo clippy --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc
```

Те же шаги на раннере — `scripts/ci/windows-build.ps1` (без флагов — релизная сборка, `-Check` — clippy и тесты), проверка бинарника — `scripts/ci/windows-smoke.ps1 <exe>` (запуск, ключ в Credential Manager, MCP `initialize` и `tools/list`), упаковка и загрузка в релиз — `scripts/release/windows-package.ps1` и `scripts/release/windows-upload.ps1`.

Ветки кода для Windows (`cfg(windows)` и `cfg(not(unix))`: временный файл `key exec`, сигналы, запуск `claude.cmd`, пути `\\?\`) компилируются только там. С macOS и Linux `cargo check --target x86_64-pc-windows-msvc` без Windows SDK не проходит: C-код `aws-lc-sys` требует заголовков Windows. Clippy можно прогнать для `x86_64-pc-windows-gnu`, взяв Zig как C-компилятор (обёртка убирает флаг `--target=…`, который cc передаёт в формате Rust):

```bash
rustup target add x86_64-pc-windows-gnu
cat > /tmp/zcc <<'SH'
#!/bin/sh
for a; do shift; case "$a" in --target=*) ;; *) set -- "$@" "$a";; esac; done
exec zig cc -target x86_64-windows-gnu "$@"
SH
chmod +x /tmp/zcc
CC_x86_64_pc_windows_gnu=/tmp/zcc AR_x86_64_pc_windows_gnu="zig ar" \
  cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
```

Тесты так не запускаются; их запускает `windows-build.ps1 -Check` на Windows.
