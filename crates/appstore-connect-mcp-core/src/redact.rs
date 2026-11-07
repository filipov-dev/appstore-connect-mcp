//! Вырезание учётных данных и лишнего содержимого из ответов App Store Connect API.
//!
//! В ответах App Store Connect встречаются значения, которым не место в контексте агента:
//! оттуда они легко попадают в логи, коммиты и документацию.
//!
//! - пароли: пароль демо-аккаунта для App Review (`demoAccountPassword`), секрет вебхука
//!   (`secret`), пароли `.p12` и прочие поля с именами паролей, токенов и ключей;
//! - адреса загрузки (`uploadOperations[].url`) и их заголовки (`requestHeaders`): одноразовые
//!   подписанные адреса; файлы загружают составные инструменты, которые получают их сами;
//! - адреса скачивания сегментов аналитических отчётов (`analyticsReportSegments`, `url`) и
//!   другие подписанные адреса (параметры `X-Amz-Signature`, `Signature`, `token` и т. п.);
//! - JWT (в том числе выпущенные клиентом, см. `ApiClient::scrub_token`), значения
//!   `Bearer …`/`Basic …`, блоки закрытых ключей PEM, пароли в URL.
//!
//! Значения заменяются на [`REDACTED`], к результату добавляется пометка
//! ([`Redactions::note`]). Кроме того, в ответах агенту ([`Mode::Response`]) большое
//! содержимое сертификатов и профилей (`certificateContent`, `profileContent`, base64) не
//! секрет, но занимает контекст: оно заменяется пояснением, как получить файл
//! (`profile_download` или `api_request` с `save_to`). В файлах `save_to` ([`Mode::File`])
//! содержимое остаётся, а секреты вырезаются так же.
//!
//! Правила:
//!
//! 1. **Поля по имени** ([`is_credential_key`]): строковое значение поля, имя которого похоже
//!    на пароль, токен, ключ или заголовок авторизации ([`CREDENTIAL_KEYS`]), на любой
//!    глубине. Имя приводится к snake_case (`demoAccountPassword` → `demo_account_password`)
//!    и сравнивается целиком или по суффиксу после `_`. Признаки (`has_…`, `is_…`) и пустые
//!    строки не трогаются.
//! 2. **Загрузка** : в элементах `uploadOperations` скрывается `url`, в списках
//!    `requestHeaders` (пары `{name, value}`) — значения всех заголовков, кроме заведомо
//!    безопасных ([`SAFE_HEADERS`]).
//! 3. **Сегменты отчётов**: у ресурса `analyticsReportSegments` скрывается `url`.
//! 4. **Значения по форме** ([`redact_secret_values`]) в любой строке: JWT, `Bearer …`,
//!    `Basic …`, PEM-блоки закрытых ключей, пароли в URL и значения параметров подписи в URL.
//!    Заменяется только сам секрет, остальной текст остаётся. Адреса следующей страницы
//!    (`links.next`) параметров подписи не содержат и не меняются.
//!
//! Для текстовых тел (не JSON) и сообщений об ошибках правило 1 применяется к парам
//! `"ключ": "строка"` в тексте (в том числе к незакрытой строке в конце, обрезанный JSON),
//! правило 4 — ко всему тексту.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use appstore_connect_api::executor::{ApiError, JsonApiError};
use regex::{Captures, Regex};
use serde_json::{Map, Value};

/// Чем заменяются скрытые значения.
pub const REDACTED: &str = "[redacted]";

/// Имена полей с учётными данными. Совпадение — имя целиком или суффикс после `_`
/// (`demo_account_password`, `p12_password`, `x_api_key`) без учёта регистра; camelCase,
/// `-`, `.` и пробелы приводятся к `_`.
pub const CREDENTIAL_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "secret",
    "shared_secret",
    "token",
    "access_token",
    "refresh_token",
    "private_key",
    "privatekey",
    "api_key",
    "apikey",
    "client_secret",
    "credentials",
    "authorization",
    "cookie",
];

/// Префиксы флагов (`has_password`, `is_secret`): это признаки, а не значения.
const FLAG_PREFIXES: &[&str] = &["has_", "is_", "uses_"];

/// Заголовки загрузки, значения которых не скрываются. Сравниваются без учёта регистра.
pub const SAFE_HEADERS: &[&str] = &[
    "accept",
    "accept-encoding",
    "cache-control",
    "content-disposition",
    "content-encoding",
    "content-length",
    "content-md5",
    "content-type",
    "date",
];

/// Поля с base64-содержимым сертификата или профиля, которое в ответе агенту заменяется
/// пояснением ([`Mode::Response`]).
pub const CONTENT_KEYS: &[&str] = &["certificateContent", "profileContent"];

/// Содержимое длиннее этого числа символов заменяется пояснением.
pub const MAX_INLINE_CONTENT: usize = 64;

/// Тип ресурса сегмента аналитического отчёта: у него подписанный адрес скачивания `url`.
const REPORT_SEGMENT_TYPE: &str = "analyticsReportSegments";

/// Куда идёт результат.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Ответ агенту: вырезаются секреты и большое содержимое сертификатов и профилей.
    Response,
    /// Файл `save_to`: вырезаются только секреты, содержимое остаётся.
    File,
}

/// Что было скрыто в одном ответе.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redactions {
    /// Имена полей и заголовков, значения которых скрыты целиком (правила 1–2).
    pub credential_fields: BTreeSet<String>,
    /// Сколько значений, похожих на секреты по форме, скрыто внутри строк (правило 4).
    pub secret_values: usize,
    /// Сколько адресов загрузки скрыто (`uploadOperations`).
    pub upload_urls: usize,
    /// Сколько адресов скачивания сегментов отчётов скрыто.
    pub report_urls: usize,
    /// Поля, содержимое которых заменено пояснением (`certificateContent`, `profileContent`).
    pub omitted_content: BTreeSet<String>,
}

impl Redactions {
    pub fn is_empty(&self) -> bool {
        self.credential_fields.is_empty()
            && self.secret_values == 0
            && self.upload_urls == 0
            && self.report_urls == 0
            && self.omitted_content.is_empty()
    }

    pub fn merge(&mut self, other: Redactions) {
        self.credential_fields.extend(other.credential_fields);
        self.secret_values += other.secret_values;
        self.upload_urls += other.upload_urls;
        self.report_urls += other.report_urls;
        self.omitted_content.extend(other.omitted_content);
    }

    /// Пояснение для агента; `None`, если ничего не скрыто.
    pub fn note(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if !self.credential_fields.is_empty() {
            let fields: Vec<&str> = self.credential_fields.iter().map(String::as_str).collect();
            parts.push(format!(
                "Values of fields that look like credentials were replaced with \"{REDACTED}\": \
                 {}.",
                fields.join(", ")
            ));
        }
        if self.secret_values > 0 {
            parts.push(format!(
                "{} value(s) that look like tokens, private keys, passwords or URL signatures \
                 were replaced with \"{REDACTED}\" inside the text.",
                self.secret_values
            ));
        }
        if self.upload_urls > 0 {
            parts.push(format!(
                "{} signed upload URL(s) (`uploadOperations`) were hidden: upload files with \
                 the `asset_upload` or `build_upload` tool, which uses them itself.",
                self.upload_urls
            ));
        }
        if self.report_urls > 0 {
            parts.push(format!(
                "{} signed download URL(s) of analytics report segments were hidden: download \
                 the reports with the `report_download` tool.",
                self.report_urls
            ));
        }
        let secrets = !self.credential_fields.is_empty() || self.secret_values > 0;
        if !self.omitted_content.is_empty() {
            let fields: Vec<&str> = self.omitted_content.iter().map(String::as_str).collect();
            parts.push(format!(
                "The base64 content of {} was omitted to keep the response small; to get the \
                 file, use `profile_download` for a provisioning profile, or save the response \
                 with `api_request` and `save_to`.",
                fields.join(", ")
            ));
        }
        let tail = if secrets || self.upload_urls > 0 || self.report_urls > 0 {
            " The server never shows hidden values; do not ask the user for them, and do not \
             try to recover them in other ways."
        } else {
            ""
        };
        Some(format!("[{}{tail}]", parts.join(" ")))
    }
}

/// Похоже ли имя поля на поле с учётными данными (см. [`CREDENTIAL_KEYS`]).
pub fn is_credential_key(key: &str) -> bool {
    let key = normalize_key(key);
    if FLAG_PREFIXES.iter().any(|prefix| key.starts_with(prefix)) {
        return false;
    }
    CREDENTIAL_KEYS.iter().any(|candidate| {
        key == *candidate
            || key
                .strip_suffix(candidate)
                .is_some_and(|rest| rest.ends_with('_'))
    })
}

fn is_safe_header(name: &str) -> bool {
    let name = name.trim();
    SAFE_HEADERS
        .iter()
        .any(|safe| safe.eq_ignore_ascii_case(name))
}

/// `demoAccountPassword`, `Access-Token`, `p12Password` → `demo_account_password`,
/// `access_token`, `p12_password`.
fn normalize_key(key: &str) -> String {
    let mut normalized = String::with_capacity(key.len() + 4);
    let mut previous: Option<char> = None;
    for c in key.trim().chars() {
        if c.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) {
            normalized.push('_');
        }
        if matches!(c, '-' | ' ' | '.') {
            normalized.push('_');
        } else {
            normalized.extend(c.to_lowercase());
        }
        previous = Some(c);
    }
    normalized
}

/// Вырезает значения из JSON (тела ответа API или результата составного инструмента).
pub fn redact_json(json: &mut Value, mode: Mode) -> Redactions {
    let mut redactions = Redactions::default();
    walk(json, mode, &mut redactions);
    redactions
}

fn walk(value: &mut Value, mode: Mode, redactions: &mut Redactions) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some(REPORT_SEGMENT_TYPE) {
                redact_report_segment(map, redactions);
            }
            for (key, item) in map.iter_mut() {
                match normalize_key(key).as_str() {
                    "upload_operations" => {
                        redact_upload_operations(item, mode, redactions);
                        continue;
                    }
                    "request_headers" => {
                        redact_headers(item, redactions);
                        continue;
                    }
                    _ => {}
                }
                match item {
                    Value::String(text) => redact_field(key, text, mode, redactions),
                    other => walk(other, mode, redactions),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, mode, redactions);
            }
        }
        Value::String(text) => {
            redactions.secret_values += redact_secret_values_in_place(text);
        }
        _ => {}
    }
}

/// Строковое значение поля `key`.
fn redact_field(key: &str, text: &mut String, mode: Mode, redactions: &mut Redactions) {
    if text.is_empty() || text == REDACTED {
        return;
    }
    if is_credential_key(key) {
        *text = REDACTED.to_owned();
        redactions.credential_fields.insert(key.to_owned());
        return;
    }
    if mode == Mode::Response
        && CONTENT_KEYS.contains(&key)
        && text.chars().count() > MAX_INLINE_CONTENT
    {
        let length = text.chars().count();
        *text = format!(
            "[omitted: {length} characters of base64 content; use `profile_download` for a \
             provisioning profile, or `api_request` with `save_to` to save the full response]"
        );
        redactions.omitted_content.insert(key.to_owned());
        return;
    }
    redactions.secret_values += redact_secret_values_in_place(text);
}

/// Элементы `uploadOperations`: `url` скрывается, заголовки — как в [`redact_headers`].
fn redact_upload_operations(value: &mut Value, mode: Mode, redactions: &mut Redactions) {
    let Value::Array(operations) = value else {
        walk(value, mode, redactions);
        return;
    };
    for operation in operations {
        let Value::Object(map) = operation else {
            walk(operation, mode, redactions);
            continue;
        };
        if let Some(Value::String(url)) = map.get_mut("url")
            && !url.is_empty()
            && url != REDACTED
        {
            *url = REDACTED.to_owned();
            redactions.upload_urls += 1;
        }
        for (key, item) in map.iter_mut() {
            if key == "url" {
                continue;
            }
            if normalize_key(key) == "request_headers" {
                redact_headers(item, redactions);
            } else {
                walk(item, mode, redactions);
            }
        }
    }
}

/// Список заголовков `[{name, value}]` (или объект `{имя: значение}`): значения всех
/// заголовков, кроме [`SAFE_HEADERS`], скрываются.
fn redact_headers(value: &mut Value, redactions: &mut Redactions) {
    match value {
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::Object(map) => redact_header_pair(map, redactions),
                    Value::String(text) => redact_header_line(text, redactions),
                    _ => {}
                }
            }
        }
        Value::Object(map) => {
            for (name, item) in map.iter_mut() {
                if let Value::String(text) = item
                    && !text.is_empty()
                    && text != REDACTED
                    && !is_safe_header(name)
                {
                    *text = REDACTED.to_owned();
                    redactions.credential_fields.insert(name.clone());
                }
            }
        }
        Value::String(text) => redact_header_line(text, redactions),
        _ => {}
    }
}

/// Заголовок в виде `{"name": "x-amz-meta", "value": "…"}`.
fn redact_header_pair(map: &mut Map<String, Value>, redactions: &mut Redactions) {
    let name = ["name", "key", "header"]
        .iter()
        .find_map(|field| map.get(*field).and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_default();
    if let Some(Value::String(value)) = map.get_mut("value")
        && !value.is_empty()
        && value != REDACTED
        && !is_safe_header(&name)
    {
        *value = REDACTED.to_owned();
        redactions.credential_fields.insert(if name.is_empty() {
            "requestHeaders".to_owned()
        } else {
            name
        });
    }
}

/// Заголовок строкой `Имя: значение`.
fn redact_header_line(text: &mut String, redactions: &mut Redactions) {
    match text.split_once(':') {
        Some((name, value))
            if !value.trim().is_empty() && value.trim() != REDACTED && !is_safe_header(name) =>
        {
            redactions.credential_fields.insert(name.trim().to_owned());
            *text = format!("{}: {REDACTED}", name.trim_end());
        }
        Some(_) => {}
        None => redactions.secret_values += redact_secret_values_in_place(text),
    }
}

/// Сегмент аналитического отчёта: подписанный адрес `url` (в `attributes` или на верхнем
/// уровне после `flatten`).
fn redact_report_segment(map: &mut Map<String, Value>, redactions: &mut Redactions) {
    let url = match map.get_mut("attributes") {
        Some(Value::Object(attributes)) => attributes.get_mut("url"),
        _ => map.get_mut("url"),
    };
    if let Some(Value::String(url)) = url
        && !url.is_empty()
        && url != REDACTED
    {
        *url = REDACTED.to_owned();
        redactions.report_urls += 1;
    }
}

/// Значения по форме (правило 4): регулярные выражения и проверка найденного значения.
struct SecretPattern {
    regex: Regex,
    /// Группа с секретом; остальной текст совпадения сохраняется.
    group: usize,
    /// Дополнительная проверка секрета: отсекает похожие обычные слова.
    accept: fn(&str) -> bool,
}

fn any_value(_: &str) -> bool {
    true
}

/// Значение после `Bearer`: токены содержат цифры, обычные слова — нет.
fn has_digit(value: &str) -> bool {
    value.chars().any(|c| c.is_ascii_digit())
}

/// Значение после `Basic`: base64 (длина кратна 4) с цифрой или выравниванием `=`.
fn looks_like_base64(value: &str) -> bool {
    value.len().is_multiple_of(4) && (value.ends_with('=') || has_digit(value))
}

static SECRET_PATTERNS: LazyLock<Vec<SecretPattern>> = LazyLock::new(|| {
    let pattern = |source: &str, group: usize, accept: fn(&str) -> bool| {
        Regex::new(source).ok().map(|regex| SecretPattern {
            regex,
            group,
            accept,
        })
    };
    [
        // Блок закрытого ключа PEM (до конца блока или до конца текста, если он обрезан).
        pattern(
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----(?s:.*?)(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
            0,
            any_value,
        ),
        // Значение заголовка авторизации: схема остаётся, токен скрывается.
        pattern(
            r"\b(?:Bearer|OAuth)\s+([A-Za-z0-9._~+/=-][A-Za-z0-9._~+/=_-]{15,})",
            1,
            has_digit,
        ),
        pattern(r"\bBasic\s+([A-Za-z0-9+/]{8,}={0,2})", 1, looks_like_base64),
        // JWT (ключ App Store Connect подписывает именно их).
        pattern(
            r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            0,
            any_value,
        ),
        // Пароль в URL: `scheme://user:пароль@host`.
        pattern(
            r#"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s/:@"'<>]+:([^\s/@"'<>]+)@"#,
            1,
            any_value,
        ),
        // Подпись и временные учётные данные в адресах (S3, CloudFront, Google Cloud Storage
        // и т. п.): значение параметра скрывается, адрес остаётся узнаваемым.
        pattern(
            r#"(?i)[?&](?:x-amz-signature|x-amz-credential|x-amz-security-token|x-goog-signature|x-goog-credential|signature|sig|policy|key-pair-id|token|access_token)=([^&#\s"'<>]+)"#,
            1,
            any_value,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
});

/// Заменяет в тексте значения, похожие на секреты по форме (правило 4). Возвращает текст
/// и число замен.
pub fn redact_secret_values(text: &str) -> (String, usize) {
    let mut current = text.to_owned();
    let count = redact_secret_values_in_place(&mut current);
    (current, count)
}

fn redact_secret_values_in_place(text: &mut String) -> usize {
    // Быстрый выход для обычных строк: у всех шаблонов есть одна из этих подстрок.
    const MARKERS: &[&str] = &["PRIVATE KEY", "Bearer", "OAuth", "Basic", "eyJ", "://", "="];
    if !MARKERS.iter().any(|marker| text.contains(marker)) {
        return 0;
    }
    let mut count = 0;
    for pattern in SECRET_PATTERNS.iter() {
        let replaced = pattern.regex.replace_all(text, |caps: &Captures<'_>| {
            let whole = caps.get(0).map_or("", |m| m.as_str());
            let Some(secret) = caps.get(pattern.group) else {
                return whole.to_owned();
            };
            if secret.as_str() == REDACTED
                || secret.as_str() == percent_redacted()
                || !(pattern.accept)(secret.as_str())
            {
                return whole.to_owned();
            }
            count += 1;
            let start = secret.start() - caps.get(0).map_or(0, |m| m.start());
            let end = start + secret.len();
            format!("{}{REDACTED}{}", &whole[..start], &whole[end..])
        });
        if let std::borrow::Cow::Owned(updated) = replaced {
            *text = updated;
        }
    }
    count
}

/// [`REDACTED`] в адресе после percent-кодирования (повторная обработка не считается).
fn percent_redacted() -> &'static str {
    "%5Bredacted%5D"
}

/// Вырезает значения из текста: строковые значения пар `"ключ": "строка"` с ключами учётных
/// данных, в том числе незакрытая строка в конце (обрезанный JSON), и значения, похожие на
/// секреты по форме.
pub fn redact_text(text: &str) -> (String, Redactions) {
    let mut redactions = Redactions::default();
    let mut text = redact_credential_pairs(text, &mut redactions);
    redactions.secret_values += redact_secret_values_in_place(&mut text);
    (text, redactions)
}

fn redact_credential_pairs(text: &str, redactions: &mut Redactions) -> String {
    let bytes = text.as_bytes();
    let skip_ws = |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut result = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while let Some(offset) = text.get(i..).and_then(|rest| rest.find('"')) {
        let start = i + offset;
        let (key_end, after_key) = string_end(bytes, start);
        i = after_key;
        let colon = skip_ws(after_key);
        if bytes.get(colon) != Some(&b':') {
            continue;
        }
        let value_start = skip_ws(colon + 1);
        if bytes.get(value_start) != Some(&b'"') {
            continue;
        }
        let (value_end, after_value) = string_end(bytes, value_start);
        i = after_value;
        let key = text.get(start + 1..key_end).unwrap_or_default();
        let value = text.get(value_start + 1..value_end).unwrap_or_default();
        if !value.is_empty() && value != REDACTED && is_credential_key(key) {
            result.push_str(text.get(copied..value_start).unwrap_or_default());
            result.push('"');
            result.push_str(REDACTED);
            result.push('"');
            copied = after_value;
            redactions.credential_fields.insert(key.to_owned());
        }
    }
    result.push_str(text.get(copied..).unwrap_or_default());
    result
}

/// Для строки, открытой кавычкой в `start`: позиция закрывающей кавычки (или конец текста)
/// и позиция сразу после строки. Обе позиции — границы символов UTF-8: это `"` или конец.
fn string_end(bytes: &[u8], start: usize) -> (usize, usize) {
    let mut j = start + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'"' => return (j, j + 1),
            _ => j += 1,
        }
    }
    (bytes.len(), bytes.len())
}

fn redact_optional(value: &mut Option<String>, redactions: &mut Redactions) {
    if let Some(text) = value {
        let (redacted, found) = redact_text(text);
        *text = redacted;
        redactions.merge(found);
    }
}

/// Ошибка API с вырезанными значениями в теле и ошибках JSON:API, и что скрыто.
pub fn redact_api_error(err: ApiError) -> (ApiError, Redactions) {
    let ApiError::Http(mut http) = err else {
        return (err, Redactions::default());
    };
    let mut redactions = Redactions::default();
    http.body = match serde_json::from_str::<Value>(&http.body) {
        Ok(mut json) => {
            let found = redact_json(&mut json, Mode::File);
            if found.is_empty() {
                http.body
            } else {
                redactions.merge(found);
                json.to_string()
            }
        }
        Err(_) => {
            let (text, found) = redact_text(&http.body);
            redactions.merge(found);
            text
        }
    };
    for error in &mut http.errors {
        let JsonApiError { title, detail, .. } = error;
        redact_optional(title, &mut redactions);
        redact_optional(detail, &mut redactions);
    }
    (ApiError::Http(http), redactions)
}

/// Текст сообщения для агента (ошибки, не связанные с телом ответа): значения по форме и
/// пары `"ключ": "строка"`. Возвращает текст с пометкой, если что-то скрыто.
pub fn redact_message(text: &str) -> String {
    let (mut text, redactions) = redact_text(text);
    if let Some(note) = redactions.note() {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    text
}

#[cfg(test)]
mod tests {
    use appstore_connect_api::executor::HttpError;
    use serde_json::json;

    use super::*;

    /// Заголовок, данные и подпись JWT по отдельности (форма настоящего, значения — нет).
    const JWT: &str = concat!(
        "eyJhbGciOiJFUzI1NiIsImtpZCI6IlRFU1QifQ",
        ".",
        "eyJpc3MiOiJ0ZXN0IiwiYXVkIjoiYXBwc3RvcmVjb25uZWN0LXYxIn0",
        ".",
        "c2lnbmF0dXJlc2lnbmF0dXJl"
    );

    #[test]
    fn credential_keys() {
        for key in [
            "password",
            "demoAccountPassword",
            "p12Password",
            "secret",
            "sharedSecret",
            "apiKey",
            "X-Api-Key",
            "Authorization",
            "refresh_token",
            "privateKey",
        ] {
            assert!(is_credential_key(key), "{key}");
        }
        for key in [
            "demoAccountName",
            "demoAccountRequired",
            "isPasswordRequired",
            "hasSecret",
            "name",
            "tokenType",
            "passTypeId",
            "publicKey",
            "keyId",
            "url",
        ] {
            assert!(!is_credential_key(key), "{key}");
        }
    }

    #[test]
    fn review_details_and_webhooks() {
        let mut body = json!({
            "data": {
                "type": "appStoreReviewDetails",
                "id": "1",
                "attributes": {
                    "demoAccountName": "reviewer@example.com",
                    "demoAccountPassword": "hunter2-review",
                    "demoAccountRequired": true,
                    "notes": "Use the demo account"
                }
            },
            "included": [{"type": "webhooks", "id": "w", "attributes": {"secret": "whsec-1", "url": "https://hooks.example.com/x"}}]
        });
        let found = redact_json(&mut body, Mode::Response);
        let attributes = &body["data"]["attributes"];
        assert_eq!(attributes["demoAccountPassword"], REDACTED);
        assert_eq!(attributes["demoAccountName"], "reviewer@example.com");
        assert_eq!(attributes["demoAccountRequired"], true);
        assert_eq!(body["included"][0]["attributes"]["secret"], REDACTED);
        assert_eq!(
            body["included"][0]["attributes"]["url"],
            "https://hooks.example.com/x"
        );
        assert_eq!(
            found.credential_fields.iter().collect::<Vec<_>>(),
            ["demoAccountPassword", "secret"]
        );
        let note = found.note().unwrap();
        assert!(note.contains("demoAccountPassword"), "{note}");
        assert!(note.contains("do not ask the user"), "{note}");
        assert!(!body.to_string().contains("hunter2"));
    }

    #[test]
    fn upload_operations_are_hidden() {
        let mut body = json!({
            "data": {
                "type": "appScreenshots",
                "id": "s1",
                "attributes": {
                    "fileName": "shot.png",
                    "uploadOperations": [{
                        "method": "PUT",
                        "url": "https://store-037.blobstore.apple.com/itmspod11-assets/x?uploadId=1&X-Amz-Signature=abc",
                        "length": 1024,
                        "offset": 0,
                        "requestHeaders": [
                            {"name": "Content-Type", "value": "image/png"},
                            {"name": "x-apple-upload-token", "value": "upload-secret-1"}
                        ]
                    }]
                }
            }
        });
        let found = redact_json(&mut body, Mode::Response);
        let operation = &body["data"]["attributes"]["uploadOperations"][0];
        assert_eq!(operation["url"], REDACTED);
        assert_eq!(operation["method"], "PUT");
        assert_eq!(operation["length"], 1024);
        assert_eq!(operation["requestHeaders"][0]["value"], "image/png");
        assert_eq!(operation["requestHeaders"][1]["value"], REDACTED);
        assert_eq!(found.upload_urls, 1);
        assert!(found.note().unwrap().contains("`asset_upload`"));
        assert!(!body.to_string().contains("blobstore"));
    }

    #[test]
    fn report_segment_urls_are_hidden() {
        let mut body = json!({"data": [{
            "type": "analyticsReportSegments",
            "id": "seg",
            "attributes": {"checksum": "abc", "sizeInBytes": 10, "url": "https://example.com/seg.csv.gz?sig=1"}
        }]});
        let found = redact_json(&mut body, Mode::Response);
        assert_eq!(body["data"][0]["attributes"]["url"], REDACTED);
        assert_eq!(body["data"][0]["attributes"]["checksum"], "abc");
        assert_eq!(found.report_urls, 1);
        assert!(found.note().unwrap().contains("`report_download`"));
    }

    #[test]
    fn content_is_omitted_only_in_responses() {
        let content = "MII".to_owned() + &"A".repeat(200);
        let body = json!({"data": {"type": "profiles", "id": "p", "attributes": {
            "name": "Dev", "profileContent": content, "certificateContent": "short"
        }}});
        let mut response = body.clone();
        let found = redact_json(&mut response, Mode::Response);
        let text = response["data"]["attributes"]["profileContent"]
            .as_str()
            .unwrap();
        assert!(text.starts_with("[omitted: 203 characters"), "{text}");
        assert_eq!(
            response["data"]["attributes"]["certificateContent"],
            "short"
        );
        let note = found.note().unwrap();
        assert!(
            note.contains("`profile_download`") && note.contains("`save_to`"),
            "{note}"
        );
        // Содержимое не секрет: без пометки про скрытые значения.
        assert!(!note.contains("do not ask"), "{note}");

        let mut file = body.clone();
        assert!(redact_json(&mut file, Mode::File).is_empty());
        assert_eq!(file, body);
    }

    #[test]
    fn secret_values_by_shape() {
        let cases = [
            (format!("token {JWT} end"), "token [redacted] end".to_owned()),
            (
                "Authorization: Bearer abcdefghijklmnop1234".to_owned(),
                "Authorization: Bearer [redacted]".to_owned(),
            ),
            (
                "Basic dXNlcjpwYXNzd29yZA==".to_owned(),
                "Basic [redacted]".to_owned(),
            ),
            (
                concat!("-----", "BEGIN PRIVATE KEY-----\nMIGT\n-----", "END PRIVATE KEY-----!").to_owned(),
                "[redacted]!".to_owned(),
            ),
            (
                concat!("-----", "BEGIN EC PRIVATE KEY-----\nMHcC (truncated").to_owned(),
                "[redacted]".to_owned(),
            ),
            (
                "https://user:pa55@host.example.com/x".to_owned(),
                "https://user:[redacted]@host.example.com/x".to_owned(),
            ),
            (
                "https://x.s3.amazonaws.com/a?X-Amz-Credential=AKIA%2F1&X-Amz-Signature=deadbeef&ok=1"
                    .to_owned(),
                "https://x.s3.amazonaws.com/a?X-Amz-Credential=[redacted]&X-Amz-Signature=[redacted]&ok=1"
                    .to_owned(),
            ),
        ];
        for (input, expected) in cases {
            let (text, count) = redact_secret_values(&input);
            assert_eq!(text, expected, "{input}");
            assert!(count >= 1, "{input}");
            // Повторная обработка ничего не меняет и не считается.
            assert_eq!(redact_secret_values(&text), (text.clone(), 0));
        }
        for plain in [
            "Bearer token",
            "Basic auth is not used",
            "https://api.appstoreconnect.apple.com/v1/apps?cursor=AB&limit=10",
            "a = b",
        ] {
            assert_eq!(
                redact_secret_values(plain),
                (plain.to_owned(), 0),
                "{plain}"
            );
        }
    }

    #[test]
    fn next_links_are_kept() {
        let next =
            "https://api.appstoreconnect.apple.com/v1/builds?cursor=eyJvZmZzZXQiOiIyIn0&limit=2";
        let mut body = json!({"data": [], "links": {"next": next}});
        assert!(redact_json(&mut body, Mode::Response).is_empty());
        assert_eq!(body["links"]["next"], next);
    }

    #[test]
    fn text_and_truncated_json() {
        let (text, found) =
            redact_text(r#"{"name": "x", "demoAccountPassword": "p1", "secret": "unterminat"#);
        assert_eq!(
            text,
            r#"{"name": "x", "demoAccountPassword": "[redacted]", "secret": "[redacted]""#
        );
        assert_eq!(found.credential_fields.len(), 2);
    }

    #[test]
    fn api_errors() {
        let err = ApiError::Http(Box::new(HttpError {
            status: 409,
            errors: vec![JsonApiError {
                id: None,
                status: Some("409".into()),
                code: Some("ENTITY_ERROR".into()),
                title: Some("Invalid".into()),
                detail: Some(format!("token {JWT} is wrong")),
                source: None,
            }],
            body: r#"{"errors": [{"detail": "x"}], "password": "p"}"#.into(),
            retry_after: None,
            rate_limit: None,
        }));
        let (err, found) = redact_api_error(err);
        let ApiError::Http(http) = err else {
            panic!("ожидалась HTTP-ошибка");
        };
        assert!(!http.body.contains("\"p\""), "{}", http.body);
        assert_eq!(
            http.errors[0].detail.as_deref(),
            Some("token [redacted] is wrong")
        );
        assert_eq!(found.secret_values, 1);
        assert!(found.credential_fields.contains("password"));

        let message = redact_message(&format!("failed with {JWT}"));
        assert!(message.starts_with("failed with [redacted]"), "{message}");
        assert!(message.contains("do not ask the user"), "{message}");
        assert_eq!(redact_message("plain"), "plain");
    }
}
