# Ключ App Store Connect API

Сервер работает с App Store Connect от имени ключа API — файла `AuthKey_<KEYID>.p8` с закрытым ключом. По ключу сервер сам выпускает короткоживущие JWT (ES256, 15 минут) и подписывает ими запросы. Своего ключа или приложения у `appstore-connect-mcp` нет: ключ создаёте вы в своём кабинете.

Подробности со стороны Apple — в документации [Creating API Keys for App Store Connect API](https://developer.apple.com/documentation/appstoreconnectapi/creating-api-keys-for-app-store-connect-api).

## Командный или индивидуальный ключ

| | Командный ключ (Team key) | Индивидуальный ключ (Individual key) |
| --- | --- | --- |
| Кто создаёт | пользователь с ролью Admin | пользователь для себя, если Admin выдал ему право Generate Individual API Keys |
| Доступ | все приложения кабинета, права — по ролям ключа | приложения и роли пользователя, которому принадлежит ключ |
| Issuer ID | есть (`--issuer-id`) | нет (`--individual`) |
| Ограничения Apple | только роли | по документации Apple: нет доступа к Provisioning (сертификаты, идентификаторы, устройства, профили), отчётам Sales и Finance и notarytool |

Сервер поддерживает оба вида. Для работы агента обычно удобнее командный ключ: он не зависит от учётной записи конкретного человека, а с индивидуальным не работают набор `provisioning` (включая `certificate_create` и `profile_download`) и отчёты продаж и финансов в `report_download`.

## Создание командного ключа

1. Откройте App Store Connect → Users and Access → Integrations → App Store Connect API → Team Keys (прямая ссылка: <https://appstoreconnect.apple.com/access/integrations/api>). Если раздел ещё не включён, владелец аккаунта (Account Holder) один раз запрашивает доступ к API.
2. Нажмите Generate API Key (или +), задайте имя (например, по агенту или машине) и роль — см. [Роли](#роли).
3. Скачайте файл `.p8`. **Apple даёт скачать его только один раз и не хранит копию.** Если файл потерян — отзовите ключ и создайте новый.
4. Запишите Key ID (в строке ключа; он же в имени файла `AuthKey_<KEYID>.p8`) и Issuer ID (над списком ключей).

`appstore-connect-mcp connect` и `appstore-connect-mcp auth login` ведут по этим шагам сами, если у профиля ещё нет ключа: открывают страницу в браузере, подсказывают, что нажать, находят скачанный файл в «Загрузках» и показывают, где взять Issuer ID.

Индивидуальный ключ создаётся в своём профиле пользователя в Users and Access (кнопка появляется, если у вас есть право Generate Individual API Keys). При сохранении укажите `--individual` или оставьте Issuer ID пустым.

## Роли

Роль ключа ограничивает, что сервер может сделать в кабинете, независимо от его уровня доступа: App Store Connect отклонит запрос с `403`, если роли не хватает. `connect` рекомендует роль **Admin**: с ней работают все инструменты, включая пользователей и роли.

Если агенту нужна только часть работы, ключ с более узкой ролью безопаснее: утёкший ключ Admin можно использовать в обход сервера. Ориентиры ниже составлены по описанию ролей Apple; точный состав прав роли меняет Apple, сверяйтесь с таблицей ролей в справке Apple Developer.

| Что нужно агенту | Наборы и инструменты | Роль, которой обычно достаточно |
| --- | --- | --- |
| Всё, включая пользователей, роли и приглашения | все, `users` | Admin |
| Версии, метаданные, скриншоты, отправка на проверку, цены и доступность, покупки и подписки | `apps`, `versions-review`, `metadata`, `media`, `pricing-availability`, `iap`, `subscriptions`, `release_status`, `metadata_*`, `asset_upload` | App Manager |
| Сборки и TestFlight | `builds`, `testflight`, `build_upload` | App Manager или Developer |
| Сертификаты, идентификаторы, профили | `provisioning`, `certificate_create`, `profile_download` | Admin, либо роль с доступом к Certificates, Identifiers & Profiles; только командный ключ |
| Отчёты продаж (Sales and Trends) | `report_download` с `kind: sales` | Sales, Finance, Admin; только командный ключ |
| Финансовые отчёты | `report_download` с `kind: finance` | Finance, Admin; только командный ключ |
| Отчёты App Analytics | `analytics-reports`, `report_download` с `kind: analytics` | Admin, App Manager или роль с доступом к отчётам |
| Отзывы и публичные ответы на них | `reviews`, промпт `triage_reviews` | Customer Support, App Manager, Admin |

Роль ключа и уровень доступа сервера — разные границы: роль решает, что разрешит App Store Connect, а `--allow-write` и `--allow-destructive` — какие инструменты сервер вообще покажет агенту ([Безопасность](security.md#уровни-доступа)).

## Сохранение ключа

```bash
appstore-connect-mcp auth login --key-file ~/Downloads/AuthKey_ABC123DEFG.p8 \
    --issuer-id 00000000-0000-0000-0000-000000000000
```

- Без флагов, в терминале, команда ведёт по шагам: от создания ключа до проверки ([пошаговый мастер](cli.md#auth-login)).
- Key ID по умолчанию берётся из имени файла `AuthKey_<KEYID>.p8`, Issuer ID — из профиля, если уже сохранён. В терминале недостающие значения спрашиваются, неверный ответ — переспрашивается.
- В скриптах и CI добавьте `--no-input`: вопросов не будет, а недостающие флаги команда перечислит в одной ошибке.
- Файл должен быть PEM PKCS#8 с ключом EC P-256 (`-----BEGIN PRIVATE KEY-----`) — именно так выглядит файл, который скачивает App Store Connect.
- Ключ проверяется запросом `GET /v1/apps?limit=1` (без проверки — `--no-verify`). Если проверка не прошла, ничего не сохраняется.
- После сохранения в терминале команда предлагает удалить исходный файл (по умолчанию — нет). Серверу файл больше не нужен, но перед удалением сохраните резервную копию, например в менеджере паролей: скачать ключ повторно нельзя.

`connect` принимает ключ так же (те же флаги `--key-file`, `--key-id`, `--issuer-id`, `--individual`, `--vendor-number`, `--no-verify`), поэтому отдельный `auth login` перед ним не нужен. Все флаги — в [справочнике CLI](cli.md#auth-login).

## Где хранится ключ

- Только в системном хранилище: Keychain в macOS, Secret Service в Linux (GNOME Keyring, KDE Wallet, KeePassXC). Сервис записи — `dev.filipov.appstore-connect-mcp`, учётная запись — имя профиля.
- Ключ не читается из переменных окружения, `.env` и аргументов командной строки и не пишется в конфигурацию MCP-клиента. Файлового хранилища нет.
- В `~/.config/appstore-connect-mcp/profiles.toml` — только несекретные настройки: Key ID, Issuer ID (или `individual = true`), vendor number, клиент и уровень доступа.
- Ни одна команда не печатает ключ. Если ключ нужен другой программе (`xcrun altool`, `xcrun notarytool`, fastlane), используйте [`key exec`](cli.md#key-exec): он отдаёт ключ дочерней команде временным файлом и удаляет его после завершения.

Подробнее — в [Безопасности](security.md#ключ-api-и-jwt).

## Профили и несколько кабинетов

Один профиль — один кабинет (одна команда App Store Connect) и один ключ. Для нескольких кабинетов заведите несколько профилей:

```bash
appstore-connect-mcp connect claude-code                    # профиль default → сервер appstore-connect
appstore-connect-mcp connect claude-code --profile client   # профиль client → сервер asc-client
```

Каждый профиль подключается к клиенту отдельным сервером: `appstore-connect` для профиля `default`, `asc-<профиль>` для остальных. Агент видит, с каким кабинетом работает каждый сервер, а инструкции сервера называют его профиль. Почему имена короткие — в [Подключении к MCP-клиентам](clients.md#имя-сервера).

Имя профиля задаётся `--profile`, иначе переменной окружения `APPSTORE_CONNECT_MCP_PROFILE`, иначе это `default`.

## Vendor number для отчётов

Отчёты продаж и финансов (`report_download` с `kind: sales` или `kind: finance`) запрашиваются по номеру поставщика (vendor number). Он указан в App Store Connect в разделе Payments and Financial Reports. Сохраните его в профиль:

```bash
appstore-connect-mcp auth login --vendor-number 80000000
```

(или `connect … --vendor-number 80000000`). Агент может передать номер и аргументом `vendor_number`; без номера инструмент возвращает ошибку с этой подсказкой.

## Отзыв ключа

Если ключ потерян, мог утечь или больше не нужен, отзовите его в App Store Connect: Users and Access → Integrations → App Store Connect API, ключ → Revoke. Отозванный ключ перестаёт работать сразу и навсегда. Затем удалите его из профиля и, если нужно, сохраните новый:

```bash
appstore-connect-mcp auth logout --profile <профиль>
appstore-connect-mcp auth login --profile <профиль> --key-file ~/Downloads/AuthKey_NEWKEY1234.p8
```

`auth logout` удаляет ключ только из системного хранилища этого компьютера: в App Store Connect ключ остаётся действующим, пока вы его не отзовёте.
