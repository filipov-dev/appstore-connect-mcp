# appstore-connect-mcp

MCP-сервер и CLI для [App Store Connect API](https://developer.apple.com/documentation/appstoreconnectapi). Даёт Claude Code, Codex и другим MCP-клиентам работать с кабинетом Apple Developer: приложения, версии и отправка на проверку, метаданные и скриншоты, сборки и TestFlight, встроенные покупки и подписки, цены и доступность, отзывы, отчёты, сертификаты и профили.

- **Все операции API — MCP-инструменты.** Они генерируются при сборке из OpenAPI-спецификации Apple; `api_search` находит нужную операцию (по-английски или по-русски), `api_request` вызывает любую в пределах уровня доступа.
- **Составные инструменты** делают за один вызов то, что в API требует нескольких шагов: загрузка скриншотов и сборок, скачивание отчётов, экспорт и применение метаданных, сводка релиза, создание сертификата и скачивание профиля.
- **Ключ API — только в системном хранилище** (Keychain в macOS, Secret Service в Linux). Его нет ни в переменных окружения, ни в `.env`, ни в конфигурации клиента; JWT выпускается в памяти процесса. Один профиль — один кабинет.
- **По умолчанию только чтение.** Запись и действия с последствиями для покупателей включаются явно; инструменты выше разрешённого уровня агенту не видны.
- **Секреты не попадают к агенту.** Ключи, JWT, пароли и временные ссылки загрузки вырезаются из ответов и ошибок.

Сервер работает через stdio: MCP-клиент сам запускает `appstore-connect-mcp serve`.

## Установка

### Установочный скрипт (Linux и macOS)

```bash
curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/appstore-connect-mcp/tarball/refs/heads/install | tar -xzO | sh
```

Скрипт ставит последнюю версию в `~/.local/bin` без `sudo` и сверяет архив с `SHA256SUMS` релиза; повторный запуск обновляет бинарник.

### Homebrew (macOS и Linux)

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/appstore-connect-mcp
```

Готовые архивы — на [странице релизов](https://sourcecraft.dev/filipov-dev/appstore-connect-mcp/releases): macOS (универсальный, подписан и нотаризован), Linux x86_64 и aarch64 (статические, без зависимостей). Параметры скрипта, проверка контрольных сумм, сборка из исходников, обновление и удаление — в [docs/install.md](docs/install.md).

## Быстрый старт

1. Создайте ключ App Store Connect API: App Store Connect → Users and Access → Integrations → App Store Connect API → Team Keys, роль — Admin (с другими ролями часть инструментов не работает). Скачайте файл `AuthKey_<KEYID>.p8` — Apple даёт скачать его только один раз. Подробнее о ключах и ролях: [docs/api-key.md](docs/api-key.md).
2. Подключите сервер к клиенту:

   ```bash
   appstore-connect-mcp connect claude-code
   # или
   appstore-connect-mcp connect codex
   ```

   `connect` спросит путь к `.p8`, Key ID и Issuer ID, проверит ключ, сохранит его в системное хранилище и после подтверждения зарегистрирует сервер `appstore-connect`. Перезапустите клиент.

Ключ можно сохранить и отдельно: `appstore-connect-mcp auth login --key-file ~/Downloads/AuthKey_ABC123DEFG.p8 --issuer-id 00000000-0000-0000-0000-000000000000`. Для второго кабинета заведите отдельный профиль: `appstore-connect-mcp connect codex --profile client` — сервер в клиенте будет называться `asc-client`. Другие клиенты и ручное подключение — [docs/clients.md](docs/clients.md).

## Уровни доступа

| Уровень | Как включить | Что доступно |
| --- | --- | --- |
| Чтение | по умолчанию | Только инструменты, которые читают данные, в том числе сводка релиза, скачивание отчётов и профилей, экспорт метаданных. |
| Запись | `--allow-write` | Создание и изменение: версии, метаданные, скриншоты, сборки, группы TestFlight, покупки, сертификаты, удаление связей. |
| Удаление | `--allow-destructive` (включает запись) | Удаление и действия с последствиями для пользователей App Store: отправка на проверку, выпуск версии и поэтапный релиз, цены, доступность по странам, предзаказы, пользователи и роли, приглашения тестировщиков, публичные ответы на отзывы. |

```bash
appstore-connect-mcp connect claude-code --allow-write
```

Инструменты выше разрешённого уровня не регистрируются: агент их не видит и не может вызвать. `--toolsets` выбирает разделы API: по умолчанию `apps`, `versions-review`, `builds`, `testflight`; `--toolsets all` включает все. `connect` по умолчанию включает динамические наборы: агент сам подключает нужные разделы. Подробнее: [docs/tools.md](docs/tools.md).

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
