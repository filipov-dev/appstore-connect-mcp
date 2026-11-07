//! Выполнение запросов к App Store Connect API.
//!
//! Исполнитель не зависит от таблицы операций: он принимает метод, относительный путь,
//! query и тело ([`ApiRequest`]). Все сгенерированные инструменты и `api_request` работают
//! через [`ApiClient::execute`]. Это граница безопасности:
//! - путь проверяется [`validate_relative_path`] (только `/v1/...`, `/v2/...` и т. д.) и
//!   приклеивается к `base_url`, поэтому запрос не уходит на другой хост;
//! - следующая страница выдачи (`links.next`, абсолютный URL) принимается только с хоста и
//!   схемы `base_url` ([`ApiClient::next_page_request`]) и тоже проходит проверку пути;
//! - заголовок `Authorization: Bearer <JWT>` формируется только здесь из [`JwtTokens`],
//!   запрос не может его переопределить; заголовок помечен как чувствительный;
//! - редиректы выполняются только в пределах того же origin;
//! - размер тела ответа ограничен `max_response_bytes`, ошибки — [`ERROR_BODY_LIMIT`];
//! - при 429 запрос повторяется с учётом `Retry-After`, при 500/502/503/504 — только для
//!   идемпотентных методов;
//! - JWT вырезается из текстов ошибок и ответов ([`ApiClient::scrub_token`]).
//!
//! Запросы на адреса, которые выдаёт сам App Store Connect (части загрузки из
//! `uploadOperations`, подписанные ссылки на файлы), сюда не входят: у них другой хост и
//! нет `Authorization`. Для них — [`crate::external`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode, redirect};
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::jwt::JwtTokens;
use crate::path::shorten;

pub use crate::path::{render_path, validate_relative_path};
/// HTTP-метод запроса и вид ответа: те же типы, что и в таблице операций.
pub use crate::spec::{HttpMethod, ResponseKind};

/// Базовый адрес App Store Connect API. Пути запросов включают версию (`/v1/...`).
pub const DEFAULT_BASE_URL: &str = "https://api.appstoreconnect.apple.com";

/// Хост App Store Connect API: только с него принимаются ссылки `links.next`.
pub const API_HOST: &str = "api.appstoreconnect.apple.com";

/// Таймаут запроса по умолчанию (одна попытка, включая чтение тела).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Таймаут установки соединения.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Лимит тела ответа по умолчанию, байт.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Максимальный размер тела ошибки (не-2xx), который читается в [`HttpError`].
pub const ERROR_BODY_LIMIT: usize = 16 * 1024;

/// Максимальное число редиректов в пределах того же origin.
const MAX_REDIRECTS: usize = 5;

/// `User-Agent` всех запросов.
pub const USER_AGENT: &str = concat!("appstore-connect-mcp/", env!("CARGO_PKG_VERSION"));

/// `Accept` для JSON-операций.
const ACCEPT_JSON: &str = "application/json, */*;q=0.8";

/// `Accept` для операций с файлом в ответе: отчёты продаж и финансов App Store Connect
/// отдаёт только как `application/a-gzip`.
const ACCEPT_BINARY: &str = "application/a-gzip, application/octet-stream;q=0.9, */*;q=0.8";

/// Символы, которые кодируются в ключах и значениях query.
///
/// Кроме unreserved-символов RFC 3986 оставляем `[`, `]` и `,`: так выглядят параметры
/// JSON:API (`fields[apps]=name,bundleId`, `filter[platform]=IOS`), как их пишет и сам
/// App Store Connect в `links.next`. `&`, `=`, `+`, `#`, `%` и пробел кодируются.
const QUERY_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'[')
    .remove(b']')
    .remove(b',');

impl HttpMethod {
    /// Повтор запроса не меняет результат. `PATCH` идемпотентным не считается: повтор после
    /// 5xx мог бы применить изменения связей дважды.
    pub fn is_idempotent(self) -> bool {
        matches!(self, Self::Get | Self::Put | Self::Delete)
    }

    pub(crate) fn to_reqwest(self) -> Method {
        match self {
            Self::Get => Method::GET,
            Self::Post => Method::POST,
            Self::Put => Method::PUT,
            Self::Patch => Method::PATCH,
            Self::Delete => Method::DELETE,
        }
    }
}

/// Политика повторов при 429 и 5xx.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Сколько раз повторять запрос после первой попытки. `0` — без повторов.
    pub max_retries: u32,
    /// Пауза, если в ответе нет `Retry-After` (удваивается с каждой попыткой).
    pub default_delay: Duration,
    /// Максимальная пауза. Если `Retry-After` больше, запрос не повторяется, а ошибка
    /// возвращается сразу с [`HttpError::retry_after`].
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            default_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Настройки клиента. `Debug` безопасен: [`JwtTokens`] не раскрывает ключ и токены.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Базовый адрес API ([`DEFAULT_BASE_URL`]). Может содержать префикс пути, он
    /// сохраняется. Допустимы `https://` и `http://` только для loopback-адресов (тесты).
    pub base_url: url::Url,
    /// Источник JWT. `None` — ключа нет: [`ApiClient::execute`] возвращает
    /// [`ApiError::MissingKey`] и ничего не отправляет.
    pub tokens: Option<Arc<JwtTokens>>,
    /// Таймаут одной попытки, включая чтение тела (и запись в `save_to`).
    pub timeout: Duration,
    /// Максимальный размер тела ответа в байтах. Больший ответ — [`ApiError::ResponseTooLarge`].
    /// На `save_to` не распространяется.
    pub max_response_bytes: usize,
    pub retry: RetryPolicy,
}

impl ClientConfig {
    /// Настройки для App Store Connect API со значениями по умолчанию.
    pub fn new(tokens: Option<Arc<JwtTokens>>) -> Result<Self, ApiError> {
        let base_url = url::Url::parse(DEFAULT_BASE_URL)
            .map_err(|e| ApiError::InvalidConfig(format!("base_url: {e}")))?;
        Ok(Self {
            base_url,
            tokens,
            timeout: DEFAULT_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            retry: RetryPolicy::default(),
        })
    }
}

/// Тело запроса.
#[derive(Debug, Clone, Default)]
pub enum RequestBody {
    #[default]
    Empty,
    /// JSON-тело, отправляется как `application/json`.
    ///
    /// Если значение — строка с JSON-объектом или массивом, она разбирается и отправляется
    /// как объект: MCP-клиенты иногда передают тело строкой, а API ждёт объект.
    Json(Value),
}

/// Запрос к API. `path` — относительный путь `/v1/...` с уже подставленными параметрами
/// (см. [`render_path`]); query передаётся отдельно в `query`. Массивы JSON:API
/// (`include`, `fields[...]`, `filter[...]`) передаются одним значением через запятую —
/// см. [`query_value`].
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: HttpMethod,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: RequestBody,
    /// Ожидаемый вид ответа: от него зависит заголовок `Accept`.
    pub response: ResponseKind,
    /// Если задан, тело успешного ответа сохраняется в файл, а не возвращается.
    ///
    /// Путь должен быть абсолютным, каталог — существовать и быть указан каноническим путём,
    /// а файла с таким именем быть не должно: существующие файлы не перезаписываются. После
    /// создания файла проверяется, что каталог не подменили ([`ApiError::LocalFileChanged`]).
    /// При ошибке частично записанный файл удаляется. Лимит `max_response_bytes` к файлу не
    /// применяется.
    pub save_to: Option<PathBuf>,
}

impl ApiRequest {
    /// Запрос без query, тела и `save_to`, с JSON-ответом.
    pub fn new(method: HttpMethod, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: Vec::new(),
            body: RequestBody::Empty,
            response: ResponseKind::Json,
            save_to: None,
        }
    }

    /// Добавляет параметр query.
    pub fn with_query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }

    /// Задаёт JSON-тело.
    pub fn with_json(mut self, body: Value) -> Self {
        self.body = RequestBody::Json(body);
        self
    }
}

/// Значение параметра query из аргумента инструмента (JSON).
///
/// Строки передаются как есть, числа и логические значения — текстом, массивы — через
/// запятую (`include=builds,appInfos`), как требует JSON:API App Store Connect. `null` —
/// параметра нет (`None`). Объекты и вложенные массивы — ошибка.
pub fn query_value(name: &str, value: &Value) -> Result<Option<String>, ApiError> {
    let scalar = |item: &Value| match item {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    match value {
        Value::Null => Ok(None),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                scalar(item).ok_or_else(|| {
                    ApiError::InvalidRequest(format!(
                        "query parameter `{name}`: array items must be strings, numbers or booleans"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|items| Some(items.join(","))),
        other => scalar(other).map(Some).ok_or_else(|| {
            ApiError::InvalidRequest(format!(
                "query parameter `{name}` must be a string, number, boolean or array"
            ))
        }),
    }
}

/// Тело ответа.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseBody {
    Empty,
    Json(Value),
    /// Текстовый ответ, который не разобрался как JSON.
    Text(String),
    /// Двоичное содержимое (например, gzip-отчёт продаж) без `save_to`: вызывающий сам
    /// решает, сохранить его или описать агенту. Не больше лимита размера ответа.
    Binary {
        bytes: Vec<u8>,
        content_type: Option<String>,
    },
    SavedToFile {
        path: PathBuf,
        bytes: u64,
        content_type: Option<String>,
    },
}

/// Постраничная выдача JSON:API App Store Connect: `links.next` и `meta.paging`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Pagination {
    /// Абсолютный адрес следующей страницы (`links.next`). Только с хоста API: ссылка на
    /// другой хост отбрасывается. Передаётся в [`ApiClient::next_page_request`] или
    /// [`ApiClient::execute_next`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// Всего записей (`meta.paging.total`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Размер страницы (`meta.paging.limit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

impl Pagination {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Поля выдачи из JSON-ответа.
    pub fn from_body(body: &Value) -> Self {
        let paging = body.pointer("/meta/paging");
        let number = |key: &str| paging.and_then(|p| p.get(key)).and_then(Value::as_u64);
        Self {
            next: body
                .pointer("/links/next")
                .and_then(Value::as_str)
                .filter(|next| !next.is_empty())
                .map(str::to_owned),
            total: number("total"),
            limit: number("limit"),
        }
    }
}

/// Квота запросов из заголовка `X-Rate-Limit` (`user-hour-lim:3600;user-hour-rem:3598;`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RateLimit {
    /// Лимит запросов за час (`user-hour-lim`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Сколько запросов осталось в текущем часе (`user-hour-rem`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<u64>,
}

impl RateLimit {
    /// Разбирает значение `X-Rate-Limit`. Неизвестные и битые пары пропускаются; если
    /// не нашлось ни лимита, ни остатка — `None`.
    pub fn parse(value: &str) -> Option<Self> {
        let mut rate = Self::default();
        for pair in value.split(';') {
            let Some((key, number)) = pair.split_once(':') else {
                continue;
            };
            let Ok(number) = number.trim().parse::<u64>() else {
                continue;
            };
            match key.trim().to_ascii_lowercase().as_str() {
                "user-hour-lim" => rate.limit = Some(number),
                "user-hour-rem" => rate.remaining = Some(number),
                _ => {}
            }
        }
        (rate != Self::default()).then_some(rate)
    }

    fn from_headers(headers: &HeaderMap) -> Option<Self> {
        header_str(headers, "x-rate-limit").and_then(Self::parse)
    }
}

/// Успешный ответ API (статус 2xx).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiResponse {
    pub status: u16,
    pub body: ResponseBody,
    pub pagination: Pagination,
    /// Квота запросов из `X-Rate-Limit`, если сервер её прислал.
    pub rate_limit: Option<RateLimit>,
}

/// Где в запросе ошибка (`errors[].source` JSON:API).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorSource {
    /// JSON Pointer на поле тела запроса (`/data/attributes/name`).
    Pointer(String),
    /// Параметр query (`filter[platform]`).
    Parameter(String),
}

impl fmt::Display for ErrorSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pointer(pointer) => write!(f, "body field `{pointer}`"),
            Self::Parameter(parameter) => write!(f, "parameter `{parameter}`"),
        }
    }
}

/// Одна ошибка из `errors[]` ответа App Store Connect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsonApiError {
    /// Идентификатор ошибки у Apple (для обращения в поддержку).
    pub id: Option<String>,
    pub status: Option<String>,
    /// Машинный код, например `ENTITY_ERROR.ATTRIBUTE.INVALID`.
    pub code: Option<String>,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub source: Option<ErrorSource>,
}

impl fmt::Display for JsonApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        if let Some(code) = &self.code {
            parts.push(code.clone());
        }
        match (&self.title, &self.detail) {
            (Some(title), Some(detail)) if detail != title => {
                parts.push(format!("{title} — {detail}"));
            }
            (Some(text), _) | (None, Some(text)) => parts.push(text.clone()),
            (None, None) => {}
        }
        let mut text = parts.join(": ");
        if let Some(source) = &self.source {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&format!("({source})"));
        }
        f.write_str(&text)
    }
}

/// Ответ App Store Connect со статусом не 2xx.
///
/// Ошибки JSON:API `{"errors": [{"id", "status", "code", "title", "detail", "source"}]}`
/// разобраны в `errors`, исходный текст (обрезанный до [`ERROR_BODY_LIMIT`]) — в `body`.
/// JWT из текстов вырезан.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpError {
    pub status: u16,
    pub errors: Vec<JsonApiError>,
    pub body: String,
    /// Через сколько секунд можно повторить запрос (`Retry-After`), если сервер сообщил.
    pub retry_after: Option<u64>,
    /// Квота запросов из `X-Rate-Limit`.
    pub rate_limit: Option<RateLimit>,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "App Store Connect API returned HTTP {}", self.status)?;
        if let Some(reason) = StatusCode::from_u16(self.status)
            .ok()
            .and_then(|s| s.canonical_reason())
        {
            write!(f, " {reason}")?;
        }
        let details: Vec<String> = self
            .errors
            .iter()
            .map(ToString::to_string)
            .filter(|text| !text.is_empty())
            .collect();
        if !details.is_empty() {
            write!(f, ": {}", details.join("; "))?;
        } else if !self.body.trim().is_empty() {
            write!(f, ": {}", self.body.trim())?;
        }
        if let Some(secs) = self.retry_after {
            write!(f, " (retry after {secs} s)")?;
        }
        if let Some(RateLimit {
            limit,
            remaining: Some(remaining),
        }) = &self.rate_limit
        {
            match limit {
                Some(limit) => write!(
                    f,
                    " (rate limit: {remaining} of {limit} requests left this hour)"
                )?,
                None => write!(f, " (rate limit: {remaining} requests left this hour)")?,
            }
        }
        Ok(())
    }
}

/// Ошибки запроса. Тексты на английском (их видит агент) и не содержат ни ключа, ни JWT.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("invalid API path: {0}")]
    InvalidPath(String),
    #[error("missing path parameter `{0}`")]
    MissingPathParam(String),
    /// Адрес следующей страницы не с хоста App Store Connect API или не проходит проверку.
    #[error(
        "the next-page URL is not accepted: {0}. Only `links.next` URLs of \
         https://{API_HOST} are followed"
    )]
    InvalidNextUrl(String),
    /// Некорректные аргументы запроса, не связанные с путём (query, `save_to`).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Некорректная конфигурация клиента (`base_url`, таймауты).
    #[error("invalid App Store Connect API client configuration: {0}")]
    InvalidConfig(String),
    /// У клиента нет ключа (см. [`ClientConfig::tokens`]); запрос не отправлялся.
    #[error("no App Store Connect API key is configured for this request")]
    MissingKey,
    /// Не удалось выпустить JWT; запрос не отправлялся.
    #[error("could not issue a JWT for the App Store Connect API: {0}")]
    Token(String),
    /// App Store Connect ответил статусом не 2xx.
    #[error("{0}")]
    Http(Box<HttpError>),
    /// Тело ответа больше лимита; ответ не возвращается.
    #[error(
        "the App Store Connect API response is larger than {limit} bytes; narrow the request \
         (fields[...], limit, filters) or save the response to a file (save_to)"
    )]
    ResponseTooLarge { limit: usize },
    /// Сеть, TLS, таймаут.
    #[error("could not reach the App Store Connect API: {0}")]
    Transport(String),
    #[error("local file error: {0}")]
    Io(String),
    /// Локальный файл для `save_to` подменили после проверки пути: путь перестал быть
    /// каноническим или указывает на другой файл. Ответ в него не записан.
    #[error("local file `{0}` was replaced after its path was checked")]
    LocalFileChanged(String),
}

impl ApiError {
    /// HTTP-статус ответа App Store Connect, если ошибка пришла от API.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http(e) => Some(e.status),
            _ => None,
        }
    }
}

/// Клиент App Store Connect API.
#[derive(Clone)]
pub struct ApiClient {
    config: ClientConfig,
    http: reqwest::Client,
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ApiClient {
    pub fn new(config: ClientConfig) -> Result<Self, ApiError> {
        check_base_url(&config.base_url)?;

        // Редиректы разрешены только в пределах origin из base_url. Внешние редиректы
        // не выполняются: так JWT и тело запроса не уходят на чужой хост, а клиент не
        // превращается в прокси. reqwest и сам убирает `Authorization` при смене хоста, но
        // мы не полагаемся только на это. Остановленный редирект возвращается как ответ 3xx
        // и превращается в `ApiError::Http`.
        let origin = config.base_url.origin();
        let policy = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if attempt.url().origin() == origin {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });

        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(config.timeout)
            .connect_timeout(CONNECT_TIMEOUT.min(config.timeout))
            .redirect(policy)
            .build()
            .map_err(|e| ApiError::InvalidConfig(describe_reqwest_error(&e)))?;

        Ok(Self { config, http })
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Заменяет JWT клиента (текущий и недавние) в тексте на `***`. Защита в глубину для
    /// текстов, которые уходят агенту: токен не должен попасть в ответ, даже если API его
    /// отразил.
    pub fn scrub_token(&self, text: String) -> String {
        self.redact(text)
    }

    /// Выполняет запрос. Не-2xx ответы возвращаются как [`ApiError::Http`].
    pub async fn execute(&self, request: ApiRequest) -> Result<ApiResponse, ApiError> {
        self.execute_with_limit(request, self.config.max_response_bytes)
            .await
    }

    /// Как [`Self::execute`], но тело ответа читается до `max_response_bytes` вместо лимита
    /// из конфигурации.
    ///
    /// Нужен, когда ответ обрабатывается перед возвратом агенту (например, фильтром в ядре
    /// MCP) или когда вызывающий сам сохраняет двоичный ответ.
    pub async fn execute_with_limit(
        &self,
        request: ApiRequest,
        max_response_bytes: usize,
    ) -> Result<ApiResponse, ApiError> {
        let Some(tokens) = &self.config.tokens else {
            return Err(ApiError::MissingKey);
        };
        let url = self.build_url(&request.path, &request.query)?;
        if let Some(save_to) = &request.save_to {
            check_save_to(save_to).await?;
        }
        let accept = HeaderValue::from_static(match request.response {
            ResponseKind::Json => ACCEPT_JSON,
            ResponseKind::Binary => ACCEPT_BINARY,
        });
        let body = match &request.body {
            RequestBody::Empty => None,
            RequestBody::Json(value) => Some(normalize_json_body(value.clone())),
        };

        tracing::debug!(
            method = request.method.as_str(),
            path = %request.path,
            "App Store Connect API request"
        );

        let retry = self.config.retry;
        let mut attempt: u32 = 0;
        let response = loop {
            // Токен берётся перед каждой попыткой: после долгой паузы на повтор он мог
            // подойти к концу срока, тогда источник выпустит новый.
            let auth = authorization(tokens)?;
            let mut builder = self
                .http
                .request(request.method.to_reqwest(), url.clone())
                .header(header::AUTHORIZATION, auth)
                .header(header::ACCEPT, accept.clone());
            if let Some(body) = &body {
                builder = builder.json(body);
            }
            let response = builder
                .send()
                .await
                .map_err(|e| ApiError::Transport(describe_reqwest_error(&e)))?;
            let status = response.status();
            if status.is_success() {
                break response;
            }

            let retry_after = parse_retry_after(response.headers());
            if is_retryable(status, request.method) && attempt < retry.max_retries {
                let delay = retry_after.unwrap_or_else(|| backoff(retry.default_delay, attempt));
                if delay <= retry.max_delay {
                    attempt += 1;
                    tracing::debug!(
                        status = status.as_u16(),
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "retrying App Store Connect API request"
                    );
                    drop(response);
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }
            return Err(self.http_error(status, retry_after, response).await);
        };

        let status = response.status();
        let rate_limit = RateLimit::from_headers(response.headers());
        let body = match &request.save_to {
            Some(path) => {
                let content_type = content_type(&response);
                let bytes = save_to_file(response, path).await?;
                ResponseBody::SavedToFile {
                    path: path.clone(),
                    bytes,
                    content_type,
                }
            }
            None => self.read_body(response, max_response_bytes).await?,
        };
        let pagination = match &body {
            ResponseBody::Json(value) => self.pagination(value),
            _ => Pagination::default(),
        };
        Ok(ApiResponse {
            status: status.as_u16(),
            body,
            pagination,
            rate_limit,
        })
    }

    /// Запрос следующей страницы по `links.next` из предыдущего ответа.
    ///
    /// Адрес принимается, только если у него та же схема, хост и порт, что у `base_url`
    /// (в работе — `https://api.appstoreconnect.apple.com`), нет логина и пароля, путь
    /// начинается с префикса `base_url` и проходит [`validate_relative_path`]. Из адреса
    /// берутся только путь и query; запрос всё равно уйдёт на `base_url`.
    pub fn next_page_request(&self, next_url: &str) -> Result<ApiRequest, ApiError> {
        let base = &self.config.base_url;
        let reject = |reason: &str| Err(ApiError::InvalidNextUrl(reason.to_owned()));
        let Ok(url) = url::Url::parse(next_url) else {
            return reject("it is not an absolute URL");
        };
        if url.scheme() != base.scheme() {
            return reject("wrong scheme");
        }
        let same_host = url
            .host_str()
            .zip(base.host_str())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b));
        if !same_host || url.port_or_known_default() != base.port_or_known_default() {
            return reject("it points to another host");
        }
        if !url.username().is_empty() || url.password().is_some() {
            return reject("it contains credentials");
        }
        let prefix = base.path().trim_end_matches('/');
        let Some(path) = url.path().strip_prefix(prefix) else {
            return reject("it is outside the API");
        };
        validate_relative_path(path)
            .map_err(|_| ApiError::InvalidNextUrl("it has an invalid API path".into()))?;
        let mut request = ApiRequest::new(HttpMethod::Get, path);
        request.query = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        Ok(request)
    }

    /// Следующая страница по `links.next` (см. [`Self::next_page_request`]).
    pub async fn execute_next(&self, next_url: &str) -> Result<ApiResponse, ApiError> {
        let request = self.next_page_request(next_url)?;
        self.execute(request).await
    }

    /// Собирает URL: префикс пути из `base_url` + проверенный относительный путь + query.
    fn build_url(&self, path: &str, query: &[(String, String)]) -> Result<url::Url, ApiError> {
        validate_relative_path(path)?;
        for (key, _) in query {
            if key.is_empty() {
                return Err(ApiError::InvalidRequest(
                    "empty query parameter name".into(),
                ));
            }
        }

        let mut url = self.config.base_url.clone();
        let prefix = url.path().trim_end_matches('/').to_string();
        // Путь уже состоит только из pchar и `%XX`, поэтому set_path его не меняет.
        url.set_path(&format!("{prefix}{path}"));
        url.set_fragment(None);
        if query.is_empty() {
            url.set_query(None);
        } else {
            let encoded: Vec<String> = query
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}={}",
                        utf8_percent_encode(key, QUERY_COMPONENT),
                        utf8_percent_encode(value, QUERY_COMPONENT)
                    )
                })
                .collect();
            url.set_query(Some(&encoded.join("&")));
        }

        // Повторная проверка: итоговый адрес обязан остаться в origin базового URL.
        if url.origin() != self.config.base_url.origin() {
            return Err(ApiError::InvalidPath(format!(
                "the path leads outside the App Store Connect API: `{}`",
                shorten(path)
            )));
        }
        Ok(url)
    }

    /// Выдача из тела ответа; `links.next` на чужой хост отбрасывается.
    fn pagination(&self, body: &Value) -> Pagination {
        let mut pagination = Pagination::from_body(body);
        if let Some(next) = &pagination.next
            && self.next_page_request(next).is_err()
        {
            tracing::debug!("links.next is not an App Store Connect API URL, dropped");
            pagination.next = None;
        }
        pagination
    }

    async fn http_error(
        &self,
        status: StatusCode,
        retry_after: Option<Duration>,
        mut response: reqwest::Response,
    ) -> ApiError {
        let mut error = HttpError {
            status: status.as_u16(),
            errors: Vec::new(),
            body: String::new(),
            retry_after: retry_after.map(|d| d.as_secs()),
            rate_limit: RateLimit::from_headers(response.headers()),
        };
        if status.is_redirection() {
            // Location не возвращаем: там может быть подписанная ссылка на внешнее хранилище.
            error.body = "a redirect to another host was not followed".to_string();
            return ApiError::Http(Box::new(error));
        }
        match read_limited(&mut response, ERROR_BODY_LIMIT).await {
            Ok((bytes, truncated)) => {
                if !truncated && let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    error.errors = parse_json_api_errors(&value)
                        .into_iter()
                        .map(|e| self.redact_error(e))
                        .collect();
                }
                let mut text = lossy_prefix(&bytes).trim().to_string();
                if truncated {
                    text.push_str(" …(truncated)");
                }
                if text.is_empty() {
                    text = status
                        .canonical_reason()
                        .unwrap_or("empty response")
                        .to_string();
                }
                error.body = self.redact(text);
            }
            Err(e) => error.body = format!("could not read the error body: {e}"),
        }
        ApiError::Http(Box::new(error))
    }

    async fn read_body(
        &self,
        mut response: reqwest::Response,
        limit: usize,
    ) -> Result<ResponseBody, ApiError> {
        let content_type = content_type(&response);
        if response
            .content_length()
            .is_some_and(|len| len > limit as u64)
        {
            return Err(ApiError::ResponseTooLarge { limit });
        }

        let (bytes, truncated) = read_limited(&mut response, limit).await?;
        if truncated {
            return Err(ApiError::ResponseTooLarge { limit });
        }
        if bytes.is_empty() {
            return Ok(ResponseBody::Empty);
        }
        let declared_binary = content_type.as_deref().is_some_and(is_binary_content_type);
        if declared_binary || looks_binary(&bytes) {
            return Ok(ResponseBody::Binary {
                bytes,
                content_type,
            });
        }

        let json_allowed = content_type
            .as_deref()
            .is_none_or(|ct| is_json_content_type(ct) || ct.starts_with("text/plain"));
        if json_allowed && let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            return Ok(ResponseBody::Json(value));
        }
        Ok(ResponseBody::Text(self.redact(lossy_prefix(&bytes))))
    }

    fn redact_error(&self, error: JsonApiError) -> JsonApiError {
        let redact = |text: Option<String>| text.map(|t| self.redact(t));
        JsonApiError {
            id: redact(error.id),
            status: redact(error.status),
            code: redact(error.code),
            title: redact(error.title),
            detail: redact(error.detail),
            source: error.source.map(|source| match source {
                ErrorSource::Pointer(p) => ErrorSource::Pointer(self.redact(p)),
                ErrorSource::Parameter(p) => ErrorSource::Parameter(self.redact(p)),
            }),
        }
    }

    /// Защита в глубину: если сервер вдруг отразил JWT в тексте, вырезаем его.
    fn redact(&self, mut text: String) -> String {
        let Some(tokens) = &self.config.tokens else {
            return text;
        };
        for token in tokens.known_tokens() {
            let token = token.expose();
            if !token.is_empty() && text.contains(token) {
                text = text.replace(token, "***");
            }
        }
        text
    }
}

/// Повторять ли запрос после ответа `status`.
///
/// 429 означает, что запрос отклонён до выполнения, поэтому повторяется для любого метода.
/// 5xx — ошибка сервера, после которой изменение могло и примениться: повторяются только
/// идемпотентные методы (`GET`, `PUT`, `DELETE`).
fn is_retryable(status: StatusCode, method: HttpMethod) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || (matches!(status.as_u16(), 500 | 502 | 503 | 504) && method.is_idempotent())
}

/// Заголовок `Authorization: Bearer <JWT>`, помеченный как sensitive.
fn authorization(tokens: &JwtTokens) -> Result<HeaderValue, ApiError> {
    let jwt = tokens
        .token()
        .map_err(|err| ApiError::Token(err.to_string()))?;
    let mut value = HeaderValue::from_str(&format!("Bearer {}", jwt.expose()))
        .map_err(|_| ApiError::Token("the JWT is not a valid header value".into()))?;
    value.set_sensitive(true);
    Ok(value)
}

fn check_base_url(base: &url::Url) -> Result<(), ApiError> {
    if base.cannot_be_a_base() || base.host().is_none() {
        return Err(ApiError::InvalidConfig(format!(
            "base_url `{base}` is not an HTTP API address"
        )));
    }
    if !base.username().is_empty() || base.password().is_some() {
        return Err(ApiError::InvalidConfig(
            "base_url must not contain a user name or password".into(),
        ));
    }
    if base.query().is_some() || base.fragment().is_some() {
        return Err(ApiError::InvalidConfig(
            "base_url must not contain a query or fragment".into(),
        ));
    }
    match base.scheme() {
        "https" => Ok(()),
        // Без TLS JWT ушёл бы открытым текстом, поэтому http — только для локальных адресов.
        "http" if is_loopback(base) => Ok(()),
        "http" => Err(ApiError::InvalidConfig(
            "http is allowed only for localhost; use https".into(),
        )),
        other => Err(ApiError::InvalidConfig(format!(
            "scheme `{other}` is not supported"
        ))),
    }
}

pub(crate) fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Пауза без `Retry-After`: `default_delay * 2^attempt`.
fn backoff(default_delay: Duration, attempt: u32) -> Duration {
    default_delay.saturating_mul(1u32 << attempt.min(16))
}

/// `Retry-After` в секундах. Формат HTTP-date не поддерживается: тогда пауза по умолчанию.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    header_str(headers, "retry-after")
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

pub(crate) fn content_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
}

/// Ошибки JSON:API из тела ответа: `errors[]` с полями `id`, `status`, `code`, `title`,
/// `detail` и `source` (`pointer` или `parameter`).
fn parse_json_api_errors(value: &Value) -> Vec<JsonApiError> {
    let text = |item: &Value, key: &str| {
        item.get(key).and_then(|v| match v {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    };
    let Some(items) = value.get("errors").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item.is_object())
        .map(|item| {
            let source = item.get("source").and_then(|source| {
                text(source, "pointer")
                    .map(ErrorSource::Pointer)
                    .or_else(|| text(source, "parameter").map(ErrorSource::Parameter))
            });
            JsonApiError {
                id: text(item, "id"),
                status: text(item, "status"),
                code: text(item, "code"),
                title: text(item, "title"),
                detail: text(item, "detail"),
                source,
            }
        })
        .collect()
}

/// Регрессия: строка с JSON-объектом или массивом отправляется как объект,
/// а не как JSON-строка с экранированным JSON внутри.
fn normalize_json_body(value: Value) -> Value {
    if let Value::String(text) = &value {
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            match serde_json::from_str::<Value>(text) {
                Ok(parsed) if parsed.is_object() || parsed.is_array() => return parsed,
                _ => {}
            }
        }
    }
    value
}

/// Проверяет `save_to` до отправки запроса, чтобы не выполнять запрос впустую.
pub(crate) async fn check_save_to(path: &Path) -> Result<(), ApiError> {
    if !path.is_absolute() {
        return Err(ApiError::InvalidRequest(format!(
            "save_to must be an absolute path: `{}`",
            path.display()
        )));
    }
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Err(ApiError::InvalidRequest(format!(
            "save_to must point to a file: `{}`",
            path.display()
        )));
    };
    match tokio::fs::metadata(parent).await {
        Ok(meta) if meta.is_dir() => {}
        _ => {
            return Err(ApiError::InvalidRequest(format!(
                "the directory for save_to does not exist: `{}`",
                parent.display()
            )));
        }
    }
    // Каталог — канонический путь (так его передаёт ядро после проверки разрешённых
    // каталогов): после создания файла это сверяется ещё раз (см. `save_to_file`).
    if tokio::fs::canonicalize(parent).await.ok().as_deref() != Some(parent) {
        return Err(ApiError::InvalidRequest(format!(
            "the save_to directory must be a canonical path without symbolic links and `..`: `{}`",
            parent.display()
        )));
    }
    if tokio::fs::symlink_metadata(path).await.is_ok() {
        return Err(ApiError::InvalidRequest(format!(
            "the file already exists and is not overwritten: `{}`",
            path.display()
        )));
    }
    Ok(())
}

/// Путь `path` канонический и сейчас указывает на тот же файл, что `opened` (метаданные
/// открытого дескриптора).
async fn ensure_same_file(path: &Path, opened: &std::fs::Metadata) -> Result<(), ApiError> {
    let changed = || ApiError::LocalFileChanged(path.display().to_string());
    let canonical = tokio::fs::canonicalize(path).await.map_err(|_| changed())?;
    if canonical != path {
        return Err(changed());
    }
    let current = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| changed())?;
    if current.file_type().is_symlink() || !same_file(&current, opened) {
        return Err(changed());
    }
    Ok(())
}

/// Один и тот же файл: устройство и inode.
#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Без inode (Windows) сравниваются тип, размер и время изменения: подмену на другой файл
/// это обнаруживает в большинстве случаев, а символическую ссылку отсекает проверка
/// канонического пути.
#[cfg(not(unix))]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.is_file() == b.is_file() && a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

pub(crate) async fn save_to_file(
    mut response: reqwest::Response,
    path: &Path,
) -> Result<u64, ApiError> {
    // create_new: не перезаписываем существующий файл и не идём по симлинку,
    // даже если он появился после check_save_to.
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|e| io_error("could not create the file", path, &e))?;
    // Каталог могли подменить символической ссылкой между проверкой и созданием файла:
    // тогда файл создан не там, где проверяли. Ничего в него не пишем и не удаляем по пути
    // (путь теперь ведёт в другое место); пустой файл остаётся, данные — нет.
    let created = file
        .metadata()
        .await
        .map_err(|e| io_error("could not read the file metadata", path, &e))?;
    ensure_same_file(path, &created).await?;

    let result = async {
        let mut written: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ApiError::Transport(describe_reqwest_error(&e)))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| io_error("could not write the file", path, &e))?;
            written += chunk.len() as u64;
        }
        file.flush()
            .await
            .map_err(|e| io_error("could not write the file", path, &e))?;
        Ok(written)
    }
    .await;

    if result.is_err() {
        drop(file);
        // Частично записанный файл бесполезен и вводит в заблуждение — удаляем.
        let _ = tokio::fs::remove_file(path).await;
    }
    result
}

/// Читает тело не больше `limit` байт. Возвращает прочитанное и признак обрезки.
pub(crate) async fn read_limited(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), ApiError> {
    let mut buf = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ApiError::Transport(describe_reqwest_error(&e)))?
    {
        let room = limit.saturating_sub(buf.len());
        if chunk.len() > room {
            buf.extend_from_slice(&chunk[..room]);
            // Остаток не дочитываем: соединение закроется при drop ответа.
            return Ok((buf, true));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((buf, false))
}

/// UTF-8 текст из байтов; незавершённый символ в конце (после обрезки) отбрасывается.
pub(crate) fn lossy_prefix(bytes: &[u8]) -> String {
    let end = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => bytes.len(),
    };
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn is_json_content_type(ct: &str) -> bool {
    let essence = ct.split(';').next().unwrap_or("").trim();
    essence == "application/json" || essence.ends_with("+json")
}

fn is_text_content_type(ct: &str) -> bool {
    let essence = ct.split(';').next().unwrap_or("").trim();
    essence.starts_with("text/")
        || is_json_content_type(essence)
        || essence.ends_with("+xml")
        || matches!(
            essence,
            "application/xml"
                | "application/javascript"
                | "application/x-www-form-urlencoded"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/x-ndjson"
        )
}

fn is_binary_content_type(ct: &str) -> bool {
    !ct.is_empty() && !is_text_content_type(ct)
}

/// Эвристика для ответов без явного двоичного типа: NUL или невалидный UTF-8.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
}

fn io_error(action: &str, path: &Path, err: &std::io::Error) -> ApiError {
    ApiError::Io(format!("{action} `{}`: {err}", path.display()))
}

/// Безопасное описание ошибки reqwest: без URL (там может быть query) и с причинами.
pub(crate) fn describe_reqwest_error(err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "timed out"
    } else if err.is_connect() {
        "could not connect"
    } else if err.is_redirect() {
        "redirect error"
    } else if err.is_body() || err.is_decode() {
        "could not read the response body"
    } else if err.is_builder() {
        "invalid request"
    } else {
        "request failed"
    };
    let mut text = kind.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn save_to_rejects_symlinked_directories_and_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let inside = base.join("inside");
        std::fs::create_dir(&inside).unwrap();

        let dir_link = inside.join("out");
        std::os::unix::fs::symlink(&outside, &dir_link).unwrap();
        let err = check_save_to(&dir_link.join("new.bin")).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");

        std::fs::write(inside.join("exists.bin"), "x").unwrap();
        let err = check_save_to(&inside.join("exists.bin")).await.unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        let err = check_save_to(Path::new("relative.bin")).await.unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
        check_save_to(&inside.join("new.bin")).await.unwrap();

        // Каталог подменили ссылкой после проверки: файл создаётся по ссылке, но это
        // обнаруживается до записи данных.
        let target_dir = inside.join("target");
        std::fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("new.bin");
        check_save_to(&target).await.unwrap();
        std::fs::remove_dir(&target_dir).unwrap();
        std::os::unix::fs::symlink(&outside, &target_dir).unwrap();
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .await
            .unwrap();
        let created = file.metadata().await.unwrap();
        let err = ensure_same_file(&target, &created).await.unwrap_err();
        assert!(matches!(err, ApiError::LocalFileChanged(_)), "{err:?}");
    }

    #[test]
    fn normalize_parses_json_strings() {
        assert_eq!(
            normalize_json_body(Value::String(r#"{"a":1}"#.into())),
            json!({"a": 1})
        );
        assert_eq!(
            normalize_json_body(Value::String(" [1,2]".into())),
            json!([1, 2])
        );
        assert_eq!(
            normalize_json_body(Value::String("plain".into())),
            Value::String("plain".into())
        );
        assert_eq!(
            normalize_json_body(Value::String("{broken".into())),
            Value::String("{broken".into())
        );
    }

    #[test]
    fn query_values() {
        assert_eq!(
            query_value("include", &json!(["builds", "appInfos"])).unwrap(),
            Some("builds,appInfos".into())
        );
        assert_eq!(query_value("limit", &json!(10)).unwrap(), Some("10".into()));
        assert_eq!(query_value("x", &json!(true)).unwrap(), Some("true".into()));
        assert_eq!(query_value("x", &json!("a b")).unwrap(), Some("a b".into()));
        assert_eq!(query_value("x", &Value::Null).unwrap(), None);
        assert!(query_value("x", &json!({"a": 1})).is_err());
        assert!(query_value("x", &json!([[1]])).is_err());
    }

    #[test]
    fn lossy_prefix_drops_cut_char() {
        let text = "ab\u{0436}";
        let bytes = text.as_bytes();
        assert_eq!(lossy_prefix(&bytes[..bytes.len() - 1]), "ab");
        assert_eq!(lossy_prefix(bytes), text);
    }

    #[test]
    fn content_type_classification() {
        assert!(is_json_content_type("application/json; charset=utf-8"));
        assert!(is_json_content_type("application/vnd.api+json"));
        assert!(is_text_content_type("text/plain"));
        assert!(is_binary_content_type("application/a-gzip"));
        assert!(is_binary_content_type("application/octet-stream"));
        assert!(!is_binary_content_type("application/xml"));
    }

    #[test]
    fn base_url_rules() {
        let ok = |s: &str| check_base_url(&url::Url::parse(s).unwrap()).is_ok();
        assert!(ok(DEFAULT_BASE_URL));
        assert!(ok("https://api.example/prefix/"));
        assert!(ok("http://127.0.0.1:8080"));
        assert!(ok("http://localhost:8080/api"));
        assert!(ok("http://[::1]:8080"));
        assert!(!ok("http://api.appstoreconnect.apple.com"));
        assert!(!ok("https://user:pass@api.example"));
        assert!(!ok("https://api.example/?x=1"));
        assert!(!ok("ftp://api.example"));
        assert!(!ok("mailto:a@b"));
    }

    #[test]
    fn default_base_url_is_the_api_host() {
        let url = url::Url::parse(DEFAULT_BASE_URL).unwrap();
        assert_eq!(url.host_str(), Some(API_HOST));
        assert_eq!(url.scheme(), "https");
    }

    #[test]
    fn retry_rules() {
        assert!(is_retryable(
            StatusCode::TOO_MANY_REQUESTS,
            HttpMethod::Post
        ));
        assert!(is_retryable(
            StatusCode::SERVICE_UNAVAILABLE,
            HttpMethod::Get
        ));
        assert!(is_retryable(StatusCode::BAD_GATEWAY, HttpMethod::Delete));
        assert!(!is_retryable(
            StatusCode::SERVICE_UNAVAILABLE,
            HttpMethod::Post
        ));
        assert!(!is_retryable(
            StatusCode::INTERNAL_SERVER_ERROR,
            HttpMethod::Patch
        ));
        assert!(!is_retryable(StatusCode::NOT_IMPLEMENTED, HttpMethod::Get));
        assert!(!is_retryable(StatusCode::CONFLICT, HttpMethod::Get));
    }

    #[test]
    fn rate_limit_parsing() {
        assert_eq!(
            RateLimit::parse("user-hour-lim:3600;user-hour-rem:3598;"),
            Some(RateLimit {
                limit: Some(3600),
                remaining: Some(3598)
            })
        );
        assert_eq!(
            RateLimit::parse(" User-Hour-Rem : 7 ; junk; x:y"),
            Some(RateLimit {
                limit: None,
                remaining: Some(7)
            })
        );
        assert_eq!(RateLimit::parse(""), None);
        assert_eq!(RateLimit::parse("other:1;"), None);
    }

    #[test]
    fn json_api_error_parsing_and_text() {
        let errors = parse_json_api_errors(&json!({
            "errors": [
                {
                    "id": "b7f1",
                    "status": "409",
                    "code": "ENTITY_ERROR.ATTRIBUTE.INVALID",
                    "title": "An attribute value is invalid.",
                    "detail": "'x' is not a valid version string.",
                    "source": {"pointer": "/data/attributes/versionString"}
                },
                {
                    "status": 400,
                    "code": "PARAMETER_ERROR.INVALID",
                    "title": "A parameter has an invalid value",
                    "source": {"parameter": "filter[platform]"}
                },
                "not an object"
            ]
        }));
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].id.as_deref(), Some("b7f1"));
        assert_eq!(errors[1].status.as_deref(), Some("400"));
        assert_eq!(
            errors[0].to_string(),
            "ENTITY_ERROR.ATTRIBUTE.INVALID: An attribute value is invalid. — 'x' is not a \
             valid version string. (body field `/data/attributes/versionString`)"
        );
        assert_eq!(
            errors[1].to_string(),
            "PARAMETER_ERROR.INVALID: A parameter has an invalid value (parameter \
             `filter[platform]`)"
        );
        assert!(parse_json_api_errors(&json!({"message": "x"})).is_empty());

        let http = HttpError {
            status: 409,
            errors,
            body: "{...}".into(),
            retry_after: None,
            rate_limit: Some(RateLimit {
                limit: Some(3600),
                remaining: Some(12),
            }),
        };
        let text = http.to_string();
        assert!(
            text.starts_with("App Store Connect API returned HTTP 409 Conflict: ENTITY_ERROR"),
            "{text}"
        );
        assert!(text.contains("; PARAMETER_ERROR.INVALID"), "{text}");
        assert!(text.contains("12 of 3600 requests left"), "{text}");
        assert!(!text.contains("{...}"), "{text}");
    }

    #[test]
    fn pagination_from_body() {
        let body = json!({
            "data": [],
            "links": {"self": "https://x/v1/apps", "next": "https://x/v1/apps?cursor=AB&limit=2"},
            "meta": {"paging": {"total": 7, "limit": 2}}
        });
        assert_eq!(
            Pagination::from_body(&body),
            Pagination {
                next: Some("https://x/v1/apps?cursor=AB&limit=2".into()),
                total: Some(7),
                limit: Some(2),
            }
        );
        assert!(Pagination::from_body(&json!({"data": {}})).is_empty());
    }

    #[test]
    fn backoff_doubles() {
        let base = Duration::from_millis(100);
        assert_eq!(backoff(base, 0), base);
        assert_eq!(backoff(base, 2), Duration::from_millis(400));
    }
}
