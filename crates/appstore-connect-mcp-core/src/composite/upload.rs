//! Общий механизм загрузки файла по `uploadOperations` App Store Connect.
//!
//! Им пользуются `asset_upload` (медиа и вложения) и `build_upload` (сборки). Порядок
//! у всех ресурсов с `uploadOperations` один:
//!
//! 1. Резервирование: `POST` ресурса с `fileName` и `fileSize`. В ответе —
//!    `uploadOperations[]`: `method`, `url` (временный подписанный адрес на хосте Apple),
//!    `offset`, `length`, `requestHeaders[]`.
//! 2. Каждая операция — отдельный запрос с диапазоном байтов файла `[offset, offset+length)`
//!    через [`ExternalClient`] (без JWT, только https). Части читаются из файла по одной,
//!    файл целиком в память не загружается; параллельно идут не больше
//!    [`MAX_PARALLEL_PARTS`] частей и не больше [`IN_FLIGHT_BUDGET`] байт (одна часть
//!    отправляется всегда, даже если она больше бюджета). Сбой сети и ответы 429/5xx
//!    повторяются [`PART_RETRIES`] раза: повтор `PUT` того же диапазона безопасен.
//! 3. Фиксация: `PATCH` с `uploaded: true` и контрольной суммой (MD5) — её делают сами
//!    инструменты, у ресурсов разные атрибуты.
//! 4. Ожидание обработки: [`poll`] с нарастающей паузой до срока.
//!
//! Безопасность: адреса и заголовки из `uploadOperations` — временные учётные данные.
//! [`UploadPart`] не выводит их в `Debug`, тексты ошибок частей содержат только номер
//! части, диапазон байтов и HTTP-статус ([`describe_external_error`]); тело ответа
//! внешнего хоста не показывается (в ответах хранилищ встречаются фрагменты подписанного
//! адреса). Ответы инструментов строятся из выбранных полей, а не копией ответа Apple.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use appstore_connect_api::executor::{
    ApiError, ApiRequest, ApiResponse, HttpMethod, ResponseBody, render_path,
};
use appstore_connect_api::external::{ExternalClient, ExternalRequest};
use md5::{Digest, Md5};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::{Context, ToolError};

/// Сколько частей отправляется одновременно, не больше.
pub(crate) const MAX_PARALLEL_PARTS: usize = 4;

/// Сколько байт частей одновременно держится в памяти (кроме случая одной большой части).
pub(crate) const IN_FLIGHT_BUDGET: u64 = 64 * 1024 * 1024;

/// Самая большая часть, которую инструмент согласен прочитать в память.
pub(crate) const MAX_PART_BYTES: u64 = 512 * 1024 * 1024;

/// Повторы одной части после сбоя сети или ответа 429/5xx.
pub(crate) const PART_RETRIES: u32 = 2;

/// Пауза перед первым повтором части (дальше удваивается).
#[cfg(not(test))]
const PART_RETRY_DELAY: Duration = Duration::from_secs(2);
#[cfg(test)]
const PART_RETRY_DELAY: Duration = Duration::from_millis(5);

/// Размер буфера при подсчёте MD5.
const HASH_BUFFER: usize = 1024 * 1024;

/// Одна операция загрузки из `uploadOperations`.
///
/// Адрес и заголовки — временные учётные данные: в `Debug` не выводятся, наружу
/// не отдаются.
pub(crate) struct UploadPart {
    pub method: HttpMethod,
    url: String,
    pub offset: u64,
    pub length: u64,
    headers: Vec<(String, String)>,
}

impl std::fmt::Debug for UploadPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadPart")
            .field("method", &self.method)
            .field("offset", &self.offset)
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

/// Разбирает `uploadOperations` из атрибутов ответа на резервирование и проверяет, что
/// части лежат внутри файла, не пересекаются и покрывают его целиком.
pub(crate) fn parse_upload_operations(
    attributes: &Value,
    file_size: u64,
) -> Result<Vec<UploadPart>, ToolError> {
    let invalid = |reason: String| {
        ToolError::new(format!(
            "App Store Connect returned invalid upload operations: {reason}"
        ))
    };
    let Some(items) = attributes
        .get("uploadOperations")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
    else {
        return Err(invalid("the list is missing or empty".into()));
    };
    let mut parts = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let number = index + 1;
        let method = match item.get("method").and_then(Value::as_str) {
            Some(m) if m.eq_ignore_ascii_case("PUT") => HttpMethod::Put,
            Some(m) if m.eq_ignore_ascii_case("POST") => HttpMethod::Post,
            Some(m) if m.eq_ignore_ascii_case("PATCH") => HttpMethod::Patch,
            _ => return Err(invalid(format!("part {number} has an unsupported method"))),
        };
        let Some(url) = item.get("url").and_then(Value::as_str) else {
            return Err(invalid(format!("part {number} has no URL")));
        };
        let offset = item.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let Some(length) = item.get("length").and_then(Value::as_u64) else {
            return Err(invalid(format!("part {number} has no length")));
        };
        if length == 0 {
            return Err(invalid(format!("part {number} is empty")));
        }
        if length > MAX_PART_BYTES {
            return Err(invalid(format!(
                "part {number} is {length} bytes, more than the supported {MAX_PART_BYTES}"
            )));
        }
        let mut headers = Vec::new();
        if let Some(list) = item.get("requestHeaders").and_then(Value::as_array) {
            for header in list {
                let name = header.get("name").and_then(Value::as_str);
                let value = header.get("value").and_then(Value::as_str);
                match (name, value) {
                    (Some(name), Some(value)) => headers.push((name.to_owned(), value.to_owned())),
                    _ => return Err(invalid(format!("part {number} has a malformed header"))),
                }
            }
        }
        parts.push(UploadPart {
            method,
            url: url.to_owned(),
            offset,
            length,
            headers,
        });
    }

    // Части должны покрывать файл без дыр и пересечений: иначе Apple получит не тот файл,
    // а проверка контрольной суммы упадёт уже после фиксации.
    let mut ranges: Vec<(u64, u64)> = parts.iter().map(|p| (p.offset, p.length)).collect();
    ranges.sort_unstable();
    let mut expected = 0u64;
    for (offset, length) in ranges {
        if offset != expected {
            return Err(invalid(
                "the parts do not cover the file contiguously".to_string(),
            ));
        }
        expected = offset.saturating_add(length);
    }
    if expected != file_size {
        return Err(invalid(format!(
            "the parts cover {expected} bytes, but the file has {file_size}"
        )));
    }
    Ok(parts)
}

/// Отправляет все части. Ошибка — текст для агента: номер части, диапазон и статус.
pub(crate) async fn upload_parts(
    external: &Arc<ExternalClient>,
    file: &Path,
    parts: Vec<UploadPart>,
) -> Result<(), String> {
    let total = parts.len();
    let largest = parts.iter().map(|p| p.length).max().unwrap_or(1).max(1);
    let parallel = usize::try_from(IN_FLIGHT_BUDGET / largest)
        .unwrap_or(MAX_PARALLEL_PARTS)
        .clamp(1, MAX_PARALLEL_PARTS);

    let mut pending = parts.into_iter().enumerate();
    let mut running = JoinSet::new();
    let mut failure: Option<(usize, String)> = None;
    loop {
        while failure.is_none() && running.len() < parallel {
            let Some((index, part)) = pending.next() else {
                break;
            };
            let external = Arc::clone(external);
            let file = file.to_path_buf();
            running.spawn(async move { (index, send_part(&external, &file, &part).await) });
        }
        let Some(joined) = running.join_next().await else {
            break;
        };
        let (index, result) = match joined {
            Ok(done) => done,
            Err(err) if err.is_cancelled() => continue,
            Err(_) => (
                usize::MAX,
                Err("the upload task stopped unexpectedly".to_owned()),
            ),
        };
        if let Err(reason) = result {
            // Сохраняем первую ошибку, остальные части отменяем: резервирование всё равно
            // будет удалено.
            if failure.is_none() {
                failure = Some((index, reason));
                running.abort_all();
            }
        }
    }
    match failure {
        None => Ok(()),
        Some((usize::MAX, reason)) => Err(reason),
        Some((index, reason)) => Err(format!("upload of part {} of {total} {reason}", index + 1)),
    }
}

/// Одна часть с повторами. Текст ошибки начинается с диапазона байтов.
async fn send_part(
    external: &ExternalClient,
    file: &Path,
    part: &UploadPart,
) -> Result<(), String> {
    let last = part.offset + part.length - 1;
    let range = format!("(bytes {}-{last})", part.offset);
    let body = read_range(file, part.offset, part.length)
        .await
        .map_err(|e| format!("{range} could not be read from the local file: {e}"))?;
    let mut attempt = 0;
    loop {
        let request = ExternalRequest {
            method: part.method,
            url: part.url.clone(),
            headers: part.headers.clone(),
            body: body.clone(),
        };
        match external.send(request).await {
            Ok(_) => return Ok(()),
            Err(err) if attempt < PART_RETRIES && is_retryable(&err) => {
                tokio::time::sleep(PART_RETRY_DELAY * 2u32.pow(attempt)).await;
                attempt += 1;
            }
            Err(err) => {
                let tries = if attempt > 0 {
                    format!(" after {} attempts", attempt + 1)
                } else {
                    String::new()
                };
                return Err(format!(
                    "{range} failed{tries}: {}",
                    describe_external_error(&err)
                ));
            }
        }
    }
}

fn is_retryable(err: &ApiError) -> bool {
    match err {
        ApiError::Transport(_) => true,
        ApiError::Http(e) => e.status == 429 || e.status >= 500,
        _ => false,
    }
}

/// Текст ошибки запроса на временный адрес: без адреса, заголовков и тела ответа.
pub(crate) fn describe_external_error(err: &ApiError) -> String {
    match err {
        ApiError::Http(e) => format!("the upload host returned HTTP {}", e.status),
        // Тексты транспорта собраны без адреса (`without_url`), проверки адреса и
        // заголовков тоже его не содержат.
        ApiError::Transport(text) => format!("network error: {text}"),
        other => other.to_string(),
    }
}

async fn read_range(file: &Path, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
    let size = usize::try_from(length)
        .map_err(|_| std::io::Error::other("the part is too large for this platform"))?;
    let mut handle = tokio::fs::File::open(file).await?;
    handle.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buffer = vec![0u8; size];
    handle.read_exact(&mut buffer).await?;
    Ok(buffer)
}

/// Размер локального файла. Пустой файл — ошибка: App Store Connect требует `fileSize >= 1`.
pub(crate) async fn file_size(file: &Path) -> Result<u64, ToolError> {
    let meta = tokio::fs::metadata(file)
        .await
        .map_err(|e| ToolError::new(format!("could not read the local file: {e}")))?;
    if !meta.is_file() {
        return Err(ToolError::new("the path is not a regular file"));
    }
    if meta.len() == 0 {
        return Err(ToolError::new("the file is empty"));
    }
    Ok(meta.len())
}

/// MD5 файла в hex (строчными), потоково.
pub(crate) async fn file_md5(file: &Path) -> Result<String, ToolError> {
    let fail = |e: std::io::Error| ToolError::new(format!("could not read the local file: {e}"));
    let mut handle = tokio::fs::File::open(file).await.map_err(fail)?;
    let mut hasher = Md5::new();
    let mut buffer = vec![0u8; HASH_BUFFER];
    loop {
        let read = handle.read(&mut buffer).await.map_err(fail)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Имя файла для `fileName`: явное или последний компонент пути.
pub(crate) fn file_name(explicit: Option<&str>, file: &Path) -> Result<String, ToolError> {
    if let Some(name) = explicit {
        let name = name.trim();
        if name.is_empty() || name.contains(['/', '\\']) {
            return Err(ToolError::new(
                "file_name must be a plain file name without directories",
            ));
        }
        return Ok(name.to_owned());
    }
    file.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::new("could not take the file name from the path; pass file_name"))
}

/// Путь экземпляра ресурса: `{collection}/{id}` с кодированием `id`.
pub(crate) fn instance_path(collection: &str, id: &str) -> Result<String, ToolError> {
    Ok(render_path(&format!("{collection}/{{id}}"), |_| {
        Some(id.to_owned())
    })?)
}

/// JSON-тело успешного ответа API.
pub(crate) fn json_body(response: ApiResponse, what: &str) -> Result<Value, ToolError> {
    match response.body {
        ResponseBody::Json(value) => Ok(value),
        _ => Err(ToolError::new(format!(
            "App Store Connect returned no JSON for {what}"
        ))),
    }
}

/// `data.id` ответа JSON:API.
pub(crate) fn data_id(body: &Value, what: &str) -> Result<String, ToolError> {
    body.pointer("/data/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolError::new(format!("App Store Connect returned no id for {what}")))
}

/// Удаляет незавершённое резервирование (без повторов ошибок наружу). Возвращает текст
/// для агента о результате.
pub(crate) async fn delete_reservation(ctx: &Context, path: &str, what: &str) -> String {
    match ctx
        .api
        .execute(ApiRequest::new(HttpMethod::Delete, path))
        .await
    {
        Ok(_) => format!("The unfinished {what} was deleted."),
        Err(err) => format!(
            "The unfinished {what} could not be deleted ({}); delete it manually.",
            ctx.api.scrub_token(err.to_string())
        ),
    }
}

/// Расписание опроса: первая пауза и максимальная.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PollSchedule {
    pub first: Duration,
    pub max: Duration,
}

/// Результат опроса.
pub(crate) struct Polled<T> {
    pub last: T,
    pub finished: bool,
}

/// Опрашивает `check`, пока он не вернёт `(значение, true)` или не выйдет `timeout`.
/// Пауза между запросами растёт в 1,5 раза до `schedule.max`. По сроку возвращается
/// последнее значение с `finished: false`.
pub(crate) async fn poll<T, F, Fut>(
    timeout: Duration,
    schedule: PollSchedule,
    mut check: F,
) -> Result<Polled<T>, ToolError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(T, bool), ToolError>>,
{
    let deadline = Instant::now() + timeout;
    let mut delay = schedule.first;
    loop {
        let (last, finished) = check().await?;
        let now = Instant::now();
        if finished || now >= deadline {
            return Ok(Polled { last, finished });
        }
        tokio::time::sleep(delay.min(deadline - now)).await;
        delay = delay.mul_f64(1.5).min(schedule.max);
    }
}

/// `timeout_seconds` из аргументов: значение по умолчанию и верхняя граница.
pub(crate) fn timeout_arg(args: &Value, default: u64, max: u64) -> Result<Duration, ToolError> {
    match args.get("timeout_seconds") {
        None | Some(Value::Null) => Ok(Duration::from_secs(default)),
        Some(value) => match value.as_u64() {
            Some(secs) if secs <= max => Ok(Duration::from_secs(secs)),
            _ => Err(ToolError::new(format!(
                "timeout_seconds must be an integer from 0 to {max}"
            ))),
        },
    }
}

/// Строковый аргумент (пустая строка — как отсутствующий).
pub(crate) fn str_arg<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Обязательный строковый аргумент.
pub(crate) fn required_str<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    str_arg(args, name).ok_or_else(|| ToolError::new(format!("`{name}` is required")))
}

/// Локальный файл для чтения по правилам сервера.
pub(crate) fn resolve_file(ctx: &Context, args: &Value) -> Result<PathBuf, ToolError> {
    ctx.files.resolve_read(required_str(args, "file")?)
}

/// Тексты `errors`/`warnings` состояния Apple (`[{code, description}]`).
pub(crate) fn state_messages(state: &Value, key: &str) -> Vec<String> {
    state
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    let code = item.get("code").and_then(Value::as_str).unwrap_or("");
                    let text = item
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    match (code.is_empty(), text.is_empty()) {
                        (false, false) => format!("{code}: {text}"),
                        (false, true) => code.to_owned(),
                        _ => text.to_owned(),
                    }
                })
                .filter(|text| !text.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Имя сгенерированного MCP-инструмента по `operationId` спецификации.
pub(crate) fn tool_for(operation_id: &str) -> Option<&'static str> {
    appstore_connect_api::spec::operations()
        .iter()
        .find(|op| op.operation_id == operation_id)
        .map(|op| op.tool_name)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Окружение для тестов загрузки: API и «хост загрузки» на одном `wiremock`.

    use std::sync::Arc;
    use std::time::Duration;

    use appstore_connect_api::executor::{ApiClient, ClientConfig, RetryPolicy};
    use appstore_connect_api::external::ExternalClient;
    use appstore_connect_api::{
        AccessLevel, ApiKey, Issuer, IssuerId, JwtTokens, KeyId, PrivateKeyPem,
    };
    use p256::ecdsa::SigningKey;
    use p256::elliptic_curve::Generate;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};

    use crate::composite::Context;
    use crate::composite::testing::{DirFiles, MemoryVault};

    /// Контекст с ключом, сгенерированным в тесте (Key ID и Issuer ID фиктивные).
    pub(crate) fn context(base: &str, dir: &std::path::Path) -> Context {
        let signing = SigningKey::generate();
        let pem = signing.to_pkcs8_pem(LineEnding::LF).unwrap();
        let pem = PrivateKeyPem::parse(&pem).unwrap();
        let key = ApiKey::new(
            &pem,
            KeyId::new("TESTKEY123").unwrap(),
            Issuer::Team(IssuerId::new("01234567-89ab-cdef-0123-456789abcdef").unwrap()),
        )
        .unwrap();
        let config = ClientConfig {
            base_url: url::Url::parse(base).unwrap(),
            tokens: Some(Arc::new(JwtTokens::new(key))),
            timeout: Duration::from_secs(5),
            max_response_bytes: 1024 * 1024,
            retry: RetryPolicy {
                max_retries: 0,
                default_delay: Duration::from_millis(5),
                max_delay: Duration::from_millis(50),
            },
        };
        Context {
            api: Arc::new(ApiClient::new(config).unwrap()),
            external: Arc::new(
                ExternalClient::new(Duration::from_secs(5), 64 * 1024)
                    .unwrap()
                    .allow_loopback_http(),
            ),
            files: Arc::new(DirFiles::new(dir)),
            vault: Arc::new(MemoryVault::default()),
            profile: Default::default(),
            access: AccessLevel::Write,
        }
    }

    /// Детерминированное содержимое файла.
    pub(crate) fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn upload_operations_must_cover_the_file() {
        let ops = json!({"uploadOperations": [
            {"method": "PUT", "url": "https://u.example/2", "offset": 5, "length": 5,
             "requestHeaders": [{"name": "Content-Type", "value": "image/png"}]},
            {"method": "PUT", "url": "https://u.example/1", "offset": 0, "length": 5},
        ]});
        let parts = parse_upload_operations(&ops, 10).unwrap();
        assert_eq!(parts.len(), 2);
        let debug = format!("{parts:?}");
        assert!(!debug.contains("u.example"), "{debug}");

        let err = parse_upload_operations(&ops, 11).unwrap_err();
        assert!(err.message.contains("cover 10 bytes"), "{err}");
        assert!(!err.message.contains("u.example"));

        let gap = json!({"uploadOperations": [
            {"method": "PUT", "url": "https://u.example/1", "offset": 0, "length": 4},
            {"method": "PUT", "url": "https://u.example/2", "offset": 5, "length": 5},
        ]});
        assert!(parse_upload_operations(&gap, 10).is_err());
        assert!(parse_upload_operations(&json!({}), 10).is_err());
        let bad_method = json!({"uploadOperations": [
            {"method": "DELETE", "url": "https://u.example/1", "offset": 0, "length": 10},
        ]});
        assert!(parse_upload_operations(&bad_method, 10).is_err());
    }

    #[tokio::test]
    async fn md5_of_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.txt");
        std::fs::write(&path, "abc").unwrap();
        assert_eq!(
            file_md5(&path).await.unwrap(),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(file_size(&path).await.unwrap(), 3);
        std::fs::write(&path, "").unwrap();
        assert!(file_size(&path).await.is_err());
    }

    #[test]
    fn file_names() {
        let path = Path::new("/tmp/dir/shot.png");
        assert_eq!(file_name(None, path).unwrap(), "shot.png");
        assert_eq!(file_name(Some("other.png"), path).unwrap(), "other.png");
        assert!(file_name(Some("../x.png"), path).is_err());
    }

    #[tokio::test]
    async fn poll_stops_at_deadline() {
        let mut calls = 0;
        let polled = poll(
            Duration::from_millis(30),
            PollSchedule {
                first: Duration::from_millis(5),
                max: Duration::from_millis(10),
            },
            || {
                calls += 1;
                let n = calls;
                async move { Ok((n, false)) }
            },
        )
        .await
        .unwrap();
        assert!(!polled.finished);
        assert!(polled.last >= 2);
    }
}
