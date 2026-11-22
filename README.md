# appstore-connect-mcp

MCP-сервер и CLI для [App Store Connect API](https://developer.apple.com/documentation/appstoreconnectapi). Даёт Claude Code, Codex и другим MCP-клиентам работать с кабинетом Apple Developer: приложения, версии и отправка на проверку, метаданные и скриншоты, сборки и TestFlight, встроенные покупки и подписки, цены и доступность, отзывы, отчёты, сертификаты и профили.

- **Все операции API — MCP-инструменты.** Они генерируются при сборке из OpenAPI-спецификации Apple; `api_search` находит нужную операцию (по-английски или по-русски), `api_request` вызывает любую в пределах уровня доступа.
- **Составные инструменты** делают за один вызов то, что в API требует нескольких шагов: загрузка скриншотов и сборок, скачивание отчётов, экспорт и применение метаданных, сводка релиза, создание сертификата и скачивание профиля.
- **Ключ API — только в системном хранилище** (Keychain в macOS, Secret Service в Linux, Credential Manager в Windows). Его нет ни в переменных окружения, ни в `.env`, ни в конфигурации клиента; JWT выпускается в памяти процесса. Один профиль — один кабинет.
- **По умолчанию только чтение.** Запись и действия с последствиями для покупателей включаются в настройках профиля; инструменты выше разрешённого уровня агенту не видны. На macOS права сверх чтения действуют только после одобрения владельцем через Touch ID или пароль.
- **Консоль и TUI.** `appstore-connect-mcp call` вызывает любой инструмент из терминала с правами профиля, `appstore-connect-mcp ui` показывает профили, их права, регистрации в клиентах и то, что видно ключу в кабинете.
- **Секреты не попадают к агенту.** Ключи, JWT, пароли и временные ссылки загрузки вырезаются из ответов и ошибок.

Сервер работает через stdio: MCP-клиент сам запускает `appstore-connect-mcp serve`.

## Установка

### Установочный скрипт (Linux и macOS)

```bash
curl -fsSL https://codeload.github.com/filipov-dev/appstore-connect-mcp/tar.gz/refs/heads/install | tar -xzO | sh
```

Скрипт ставит последнюю версию в `~/.local/bin` без `sudo` и сверяет архив с `SHA256SUMS` релиза; повторный запуск обновляет бинарник.

### Homebrew (macOS и Linux)

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/appstore-connect-mcp
```

Готовые архивы — на [странице релизов на GitHub](https://github.com/filipov-dev/appstore-connect-mcp/releases) (код — в [SourceCraft](https://sourcecraft.dev/filipov-dev/appstore-connect-mcp) и в его копии на GitHub): macOS (универсальный, подписан и нотаризован), Linux x86_64 и aarch64 (статические, без зависимостей), Windows x86_64 (`.zip`, без зависимостей, не подписан: SmartScreen может предупредить о неизвестном издателе; установка в PowerShell — в [docs/install.md](docs/install.md#windows)). Параметры скрипта, проверка контрольных сумм, сборка из исходников, обновление и удаление — в [docs/install.md](docs/install.md).

## Быстрый старт

Подключите сервер к клиенту:

```bash
appstore-connect-mcp connect claude-code
# или
appstore-connect-mcp connect codex
```

Если ключа App Store Connect API ещё нет, `connect` проведёт по шагам: объяснит, что такое файл ключа, Key ID и Issuer ID, откроет в браузере страницу ключей App Store Connect и подскажет, что нажать (командный ключ, роль Admin, файл `.p8` скачивается только один раз), найдёт скачанный `AuthKey_<KEYID>.p8` в «Загрузках», покажет, где взять Issuer ID, проверит ключ и сохранит его в системное хранилище. Затем после подтверждения зарегистрирует сервер `appstore-connect`. Перезапустите клиент. Подробнее о ключах и ролях: [docs/api-key.md](docs/api-key.md).

Только сохранить ключ, без регистрации в клиенте, — `appstore-connect-mcp auth login` (тот же пошаговый мастер). Для скриптов и CI есть режим без вопросов: `appstore-connect-mcp auth login --no-input --key-file ~/Downloads/AuthKey_ABC123DEFG.p8 --issuer-id 00000000-0000-0000-0000-000000000000`. Для второго кабинета заведите отдельный профиль: `appstore-connect-mcp connect codex --profile client` — сервер в клиенте будет называться `asc-client`. Другие клиенты и ручное подключение — [docs/clients.md](docs/clients.md).

## Уровни доступа

| Уровень | В профиле | Что доступно |
| --- | --- | --- |
| Чтение | по умолчанию | Только инструменты, которые читают данные, в том числе сводка релиза, скачивание отчётов и профилей, экспорт метаданных. |
| Запись | `allow_write = true` | Создание и изменение: версии, метаданные, скриншоты, сборки, группы TestFlight, покупки, сертификаты, удаление связей. |
| Удаление | `allow_destructive = true` (включает запись) | Удаление и действия с последствиями для пользователей App Store: отправка на проверку, выпуск версии и поэтапный релиз, цены, доступность по странам, предзаказы, пользователи и роли, приглашения тестировщиков, публичные ответы на отзывы. |

Уровень доступа и дополнительные каталоги для файлов (`allow_path`) — настройки профиля. Их меняет `appstore-connect-mcp ui` (клавиши `r`, `w`, `d`, затем `s`) или `connect` с флагом:

```bash
appstore-connect-mcp connect claude-code --allow-write
```

В конфигурацию клиента флаги доступа не попадают: клиент запускает `serve --profile <имя>`, а `serve` берёт права из профиля. Флаги `serve` (`--allow-write`, `--allow-destructive`, `--allow-path`) могут только сузить права профиля. На macOS права сверх чтения нужно одобрить: при сохранении `ui` и `connect` спрашивают Touch ID или пароль пользователя macOS, а без одобрения сервер работает только на чтение и пишет об этом в stderr. Так агент, который может править файлы, не расширит себе права правкой `profiles.toml` или конфигурации клиента. В Linux и Windows одобрения нет: границу задаёт роль ключа в App Store Connect. Подробнее: [docs/security.md](docs/security.md#уровни-доступа).

Инструменты выше разрешённого уровня не регистрируются: агент их не видит и не может вызвать. `--toolsets` выбирает разделы API: по умолчанию `apps`, `versions-review`, `builds`, `testflight`; `--toolsets all` включает все. `connect` по умолчанию включает динамические наборы: агент сам подключает нужные разделы. Подробнее: [docs/tools.md](docs/tools.md).

## Консоль и TUI

```bash
appstore-connect-mcp call                       # инструменты, доступные профилю
appstore-connect-mcp call list_apps fields.apps=name,bundleId limit=5
appstore-connect-mcp call get_app --schema      # схема аргументов
appstore-connect-mcp ui                         # профили, права, регистрации, ключ
```

`call` собирает сервер с теми же правами, что и `serve`, и вызывает его в этом же процессе: проверка аргументов, уровни доступа и вырезание секретов те же, что у агента. Инструменты уровня удаления спрашивают подтверждение (`--yes` — без вопроса). Подробнее: [docs/cli.md](docs/cli.md#call).

## Документация

- [Установка и обновление](docs/install.md)
- [Ключ App Store Connect API](docs/api-key.md)
- [Подключение к MCP-клиентам](docs/clients.md)
- [Справочник CLI](docs/cli.md)
- [MCP-инструменты](docs/tools.md)
- [Безопасность](docs/security.md)
- [Решение проблем](docs/troubleshooting.md)
- [Разработка](docs/development.md)

## Лицензия

`MIT OR Apache-2.0` — на ваш выбор: [LICENSE-MIT](LICENSE-MIT) или [LICENSE-APACHE](LICENSE-APACHE).
