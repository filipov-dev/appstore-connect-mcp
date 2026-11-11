# Установка и обновление

`appstore-connect-mcp` — один бинарник: и MCP-сервер (`serve`), и CLI для настройки (`connect`, `auth`, `key exec`). Готовые сборки есть для macOS, Linux и Windows.

## Платформы

| Платформа | Сборка | Хранилище ключа |
| --- | --- | --- |
| macOS, Apple silicon и Intel | универсальный бинарник, подписан Developer ID и нотаризован | Keychain |
| Linux x86_64 | статический бинарник (musl), без зависимостей | Secret Service |
| Linux aarch64 | статический бинарник (musl), без зависимостей | Secret Service |
| Windows x86_64 | `appstore-connect-mcp.exe` без зависимостей (CRT встроен), не подписан | Credential Manager |

Linux-бинарник собран статически: ему не нужны glibc определённой версии, `libdbus-1`, OpenSSL и другие системные библиотеки, он запускается в любом дистрибутиве, включая Alpine и slim-образы. Для HTTPS-запросов к App Store Connect нужны корневые сертификаты системы (пакет `ca-certificates`).

В Linux ключ хранится через Secret Service (GNOME Keyring, KDE Wallet, KeePassXC и т. п.): провайдер должен быть запущен в сессии D-Bus пользователя. Запасного файлового хранилища нет — см. [Linux без Secret Service](troubleshooting.md#linux-secret-service-недоступен).

## Установочный скрипт

Одна команда для Linux (x86_64, aarch64) и macOS, Homebrew не нужен:

```bash
curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/appstore-connect-mcp/tarball/refs/heads/install | tar -xzO | sh
```

Адрес — архив ветки `install` публичного репозитория: в ней лежит только `install.sh` со списком релизов (адреса архивов и их SHA-256), его обновляет релизный конвейер. `tar -xzO` печатает скрипт, `sh` выполняет.

Что делает скрипт:

- определяет ОС и архитектуру и выбирает архив релиза (`linux-x86_64`, `linux-aarch64` или `macos-universal`);
- скачивает архив и `SHA256SUMS` релиза и сверяет сумму архива; при расхождении ничего не устанавливает;
- кладёт бинарник в `~/.local/bin/appstore-connect-mcp` без `sudo`; замена атомарная, уже запущенный сервер продолжает работать со старой копией;
- если каталога нет в `PATH`, подсказывает, как его добавить.

Повторный запуск обновляет бинарник до последней версии (если она уже стоит — ничего не скачивает). Путь не меняется, поэтому `connect` повторять не нужно.

Параметры передаются через `sh -s --`:

```bash
# Конкретная версия
curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/appstore-connect-mcp/tarball/refs/heads/install | tar -xzO | sh -s -- --version v0.1.0

# Другой каталог: бинарник попадёт в /opt/asc/bin
curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/appstore-connect-mcp/tarball/refs/heads/install | tar -xzO | sh -s -- --prefix /opt/asc
```

| Параметр | Что делает |
| --- | --- |
| `--version vX.Y.Z` | Версия вместо последней. Список — `--list`. |
| `--prefix DIR` | Установить в `DIR/bin` (по умолчанию `~/.local`). |
| `--bin-dir DIR` | Установить ровно в `DIR` (важнее `--prefix`). |
| `--force` | Переустановить, даже если эта версия уже стоит. |
| `--list` | Показать доступные версии и выйти. |

Нужны `sh`, `tar`, `curl` или `wget`, `sha256sum` или `shasum`. Чтобы прочитать скрипт перед запуском, сохраните его в файл: `curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/appstore-connect-mcp/tarball/refs/heads/install | tar -xzO > install.sh`.

## Homebrew

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/appstore-connect-mcp
appstore-connect-mcp --version
```

- Tap размещён на SourceCraft, а не на GitHub, поэтому `brew tap` нужен полный адрес репозитория.
- Формулу устанавливайте по полному имени `filipov-dev/tap/appstore-connect-mcp`: так Homebrew фиксирует, что вы доверяете формуле из стороннего tap.
- В Linux формула ставит статический бинарник, зависимостей у неё нет.

Обновление и удаление:

```bash
brew update && brew upgrade filipov-dev/tap/appstore-connect-mcp
brew uninstall filipov-dev/tap/appstore-connect-mcp
```

`connect` записывает в конфигурацию клиента путь, по которому был запущен бинарник, не раскрывая символических ссылок. Если запускать его как `appstore-connect-mcp` из `PATH` Homebrew, в записи окажется ссылка `…/bin/appstore-connect-mcp`, которая переживает `brew upgrade`. Если в записи клиента путь внутри `Cellar` (с номером версии), после обновления повторите `connect`.

## Готовый архив

Архивы и файл `SHA256SUMS` публикуются на [странице релизов](https://sourcecraft.dev/filipov-dev/appstore-connect-mcp/releases). Имя архива: `appstore-connect-mcp-<тег>-<платформа>.tar.gz`, например `appstore-connect-mcp-v0.1.0-linux-x86_64.tar.gz`. Внутри — каталог с бинарником `appstore-connect-mcp`, `README.md` и лицензиями. Архив для Windows — `.zip`, его установка описана в разделе [Windows](#windows).

Проверьте контрольную сумму (из каталога, куда скачаны архив и `SHA256SUMS`):

```bash
# Linux
grep linux-x86_64 SHA256SUMS | sha256sum -c -

# macOS
grep macos-universal SHA256SUMS | shasum -a 256 -c -
```

Распакуйте и установите бинарник в каталог из `PATH`:

```bash
tar -xzf appstore-connect-mcp-v0.1.0-linux-x86_64.tar.gz
sudo install -m 0755 appstore-connect-mcp-v0.1.0-linux-x86_64/appstore-connect-mcp /usr/local/bin/
appstore-connect-mcp --version
```

Чтобы обновиться, замените бинарник новым по тому же пути: конфигурацию клиентов трогать не нужно.

На macOS проверить подпись можно так (в выводе второй команды — строка `Authority=Developer ID Application: …`):

```bash
codesign --verify --strict --verbose=2 /usr/local/bin/appstore-connect-mcp
codesign -dv --verbose=2 /usr/local/bin/appstore-connect-mcp
```

## Windows

Для Windows есть только архив `appstore-connect-mcp-<тег>-windows-x86_64.zip` на [странице релизов](https://sourcecraft.dev/filipov-dev/appstore-connect-mcp/releases): установочного скрипта, Homebrew и пакетов для менеджеров пакетов нет. Внутри — каталог с `appstore-connect-mcp.exe`, `README.md` и лицензиями. Бинарнику не нужен Visual C++ Redistributable и другие библиотеки. Ключи хранятся в Windows Credential Manager (общие учётные данные `<профиль>.dev.filipov.appstore-connect-mcp`).

Скачайте архив и `SHA256SUMS` в один каталог и выполните в PowerShell из него (тег замените на нужный):

```powershell
$tag = 'v0.2.1'
$zip = "appstore-connect-mcp-$tag-windows-x86_64.zip"

# Контрольная сумма: значение должно совпасть со строкой windows-x86_64 в SHA256SUMS
(Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
Select-String -SimpleMatch $zip SHA256SUMS

# Распаковка и установка в профиль пользователя (права администратора не нужны)
Unblock-File $zip
Expand-Archive $zip -DestinationPath "$env:TEMP\appstore-connect-mcp" -Force
$dir = "$env:LOCALAPPDATA\Programs\appstore-connect-mcp"
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item "$env:TEMP\appstore-connect-mcp\appstore-connect-mcp-$tag-windows-x86_64\appstore-connect-mcp.exe" $dir -Force
```

Добавьте каталог в `PATH` пользователя (один раз) и откройте новое окно терминала:

```powershell
$dir = "$env:LOCALAPPDATA\Programs\appstore-connect-mcp"
$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($path -split ';') -notcontains $dir) {
  [Environment]::SetEnvironmentVariable('Path', "$path;$dir", 'User')
}
```

```powershell
appstore-connect-mcp --version
```

Бинарник не подписан сертификатом подписи кода. Если открыть его из Проводника или из браузера, SmartScreen может показать «Система Windows защитила ваш компьютер»: проверьте контрольную сумму и нажмите «Подробнее» → «Выполнить в любом случае». `Unblock-File` снимает отметку «загружено из интернета» с архива, и распакованный бинарник её не получает. В Windows 11 со включённым Smart App Control неподписанные программы блокируются без возможности запуска — используйте другую машину или соберите бинарник из исходников.

Чтобы обновиться, замените `appstore-connect-mcp.exe` новым по тому же пути: конфигурацию клиентов трогать не нужно. Пока MCP-клиент держит сервер запущенным, файл занят — закройте клиент или сначала переименуйте старый файл (`Rename-Item appstore-connect-mcp.exe appstore-connect-mcp.old.exe`; переименовать запущенный файл Windows разрешает) и удалите его после перезапуска клиента.

Настройки профилей лежат в `%APPDATA%\appstore-connect-mcp\profiles.toml`. Инструменты сервера работают так же, как на других системах, кроме установки профиля в Xcode (`profile_download` с `install`): она есть только на macOS, профиль можно сохранить через `save_to`. Команд Apple (`xcrun`, Transporter) в Windows нет: `key exec` пригодится для fastlane и других кроссплатформенных инструментов. У временного файла ключа в `key exec` нет прав Unix: он лежит во временном каталоге профиля пользователя (`%TEMP%`), закрытом от других пользователей.

## Из исходников

Нужен Rust 1.88 или новее (`rustup` подхватит канал из `rust-toolchain.toml`) и C-компилятор для криптографии TLS. Системные библиотеки D-Bus и OpenSSL не нужны.

```bash
cargo build --release
```

Бинарник: `target/release/appstore-connect-mcp` (в Windows — `target\release\appstore-connect-mcp.exe`). Скопируйте его в каталог из `PATH` или установите сразу:

```bash
cargo install --path crates/appstore-connect-mcp
```

В Windows нужны Visual Studio Build Tools с компонентом «Разработка классических приложений на C++» (компилятор и компоновщик MSVC). Релизный архив собирается так: `cargo build --release --target x86_64-pc-windows-msvc` с `RUSTFLAGS=-C target-feature=+crt-static` — тогда бинарнику не нужен Visual C++ Redistributable.

На macOS собранный локально бинарник не подписан Developer ID: после пересборки Keychain может снова спросить разрешение на доступ к ключу ([Решение проблем](troubleshooting.md#macos-keychain-спрашивает-разрешение)). Статическая сборка под Linux — в [Разработке](development.md#статическая-сборка-под-linux).

## Проверка

```bash
appstore-connect-mcp --version
appstore-connect-mcp toolsets          # наборы инструментов; ключ и сеть не нужны
```

После сохранения ключа:

```bash
appstore-connect-mcp auth status       # ключ в хранилище и проверочный запрос к API
appstore-connect-mcp serve --status    # с какими настройками запустится сервер
```

## Первое подключение

После установки создайте ключ API ([Ключ App Store Connect API](api-key.md)) и подключите сервер к клиенту: `appstore-connect-mcp connect claude-code` или `appstore-connect-mcp connect codex` ([Подключение к MCP-клиентам](clients.md)).

## Удаление

1. Отключите сервер в клиентах: `claude mcp remove --scope user appstore-connect` (или `asc-<профиль>`), таблица `[mcp_servers.appstore-connect]` в `~/.codex/config.toml` или запись в конфигурации другого клиента.
2. Удалите ключи и настройки профилей: `appstore-connect-mcp auth logout --profile <профиль> --forget` для каждого профиля.
3. Если `certificate_create` сохранял приватные ключи сертификатов и пароли `.p12`, они лежат в системном хранилище под сервисом `dev.filipov.appstore-connect-mcp.secrets`: удалите их сами (в macOS — в Keychain Access, в Linux — в менеджере связки ключей, например Seahorse, в Windows — в «Диспетчере учётных данных», раздел «Учётные данные Windows»), предварительно убедившись, что ключи сертификатов больше не нужны.
4. Отзовите ключи API, которые больше не нужны, в App Store Connect ([Отзыв ключа](api-key.md#отзыв-ключа)).
5. Удалите бинарник (`brew uninstall filipov-dev/tap/appstore-connect-mcp` или файл из `PATH`) и, если осталась, папку `~/.config/appstore-connect-mcp` (в Windows — `%APPDATA%\appstore-connect-mcp`).
