//! Запросы на адреса, которые выдаёт сам App Store Connect, а не на API.
//!
//! Составным инструментам нужны два вида таких запросов:
//! - загрузка частей файла (скриншоты, превью, сборки) по `uploadOperations` из ответа
//!   на резервирование: `method`, `url` (произвольный https-хост Apple), `requestHeaders`
//!   и диапазон байтов файла;
//! - скачивание по подписанным ссылкам (сегменты отчётов аналитики и т. п.).
//!
//! Это отдельный клиент, а не [`crate::executor::ApiClient`]: у этих адресов другой хост,
//! и заголовка `Authorization` (JWT) у них быть не должно — подпись уже в самом адресе.
//! Правила:
//! - только `https://` (для тестов — `http://` на loopback, если явно разрешено);
//! - заголовки из ответа Apple передаются как есть, кроме служебных и авторизационных
//!   (`Authorization`, `Cookie`, `Host`, ...) — они отклоняются;
//! - редиректы не выполняются;
//! - адрес (в нём подпись) не попадает в тексты ошибок и логи.

use std::path::Path;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use reqwest::redirect;

use crate::executor::{
    ApiError, CONNECT_TIMEOUT, HttpError, HttpMethod, USER_AGENT, check_save_to, content_type,
    describe_reqwest_error, is_loopback, lossy_prefix, read_limited, save_to_file,
};

/// Сколько байт тела ошибки внешнего хоста читается в [`HttpError::body`].
const EXTERNAL_ERROR_BODY_LIMIT: usize = 1024;

/// Заголовки, которые нельзя задать запросу на внешний адрес.
const FORBIDDEN_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
];

/// Запрос на адрес, выданный App Store Connect (часть загрузки из `uploadOperations`).
#[derive(Clone)]
pub struct ExternalRequest {
    pub method: HttpMethod,
    /// Абсолютный адрес из ответа API. В `Debug` не выводится: в нём подпись.
    pub url: String,
    /// `requestHeaders` из ответа API (имя, значение).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for ExternalRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalRequest")
            .field("method", &self.method)
            .field("url", &"***")
            .field("headers", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Ответ внешнего хоста со статусом 2xx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// Клиент для адресов, выданных App Store Connect. Без JWT.
#[derive(Debug, Clone)]
pub struct ExternalClient {
    http: reqwest::Client,
    allow_loopback_http: bool,
    max_response_bytes: usize,
}

impl ExternalClient {
    /// Клиент с таймаутом одной попытки `timeout` и лимитом тела ответа
    /// `max_response_bytes` (на [`Self::download_to`] не распространяется).
    pub fn new(timeout: Duration, max_response_bytes: usize) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT.min(timeout))
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|e| ApiError::InvalidConfig(describe_reqwest_error(&e)))?;
        Ok(Self {
            http,
            allow_loopback_http: false,
            max_response_bytes,
        })
    }

    /// Разрешает `http://` на loopback-адресах (только для тестов с локальным сервером).
    pub fn allow_loopback_http(mut self) -> Self {
        self.allow_loopback_http = true;
        self
    }

    fn check_url(&self, url: &str) -> Result<url::Url, ApiError> {
        let invalid = |reason: &str| {
            Err(ApiError::InvalidRequest(format!(
                "upload or download URL: {reason}"
            )))
        };
        let Ok(parsed) = url::Url::parse(url) else {
            return invalid("not an absolute URL");
        };
        match parsed.scheme() {
            "https" => {}
            "http" if self.allow_loopback_http && is_loopback(&parsed) => {}
            _ => return invalid("only https URLs are allowed"),
        }
        if parsed.host().is_none() {
            return invalid("no host");
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return invalid("credentials in the URL are not allowed");
        }
        Ok(parsed)
    }

    fn request(
        &self,
        method: HttpMethod,
        url: url::Url,
        headers: &[(String, String)],
    ) -> Result<reqwest::RequestBuilder, ApiError> {
        let mut builder = self.http.request(method.to_reqwest(), url);
        for (name, value) in headers {
            let lower = name.trim().to_ascii_lowercase();
            if FORBIDDEN_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-") {
                return Err(ApiError::InvalidRequest(format!(
                    "header `{lower}` cannot be set on an upload or download request"
                )));
            }
            let name = HeaderName::from_bytes(lower.as_bytes()).map_err(|_| {
                ApiError::InvalidRequest(format!("invalid header name `{}`", name.escape_debug()))
            })?;
            let value = HeaderValue::from_str(value).map_err(|_| {
                ApiError::InvalidRequest(format!("invalid value of header `{name}`"))
            })?;
            builder = builder.header(name, value);
        }
        Ok(builder)
    }

    /// Отправляет запрос (например, часть файла по `uploadOperations`). Не-2xx —
    /// [`ApiError::Http`] с началом тела ответа.
    pub async fn send(&self, request: ExternalRequest) -> Result<ExternalResponse, ApiError> {
        let url = self.check_url(&request.url)?;
        let builder = self
            .request(request.method, url, &request.headers)?
            .body(request.body);
        let mut response = builder
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_reqwest_error(&e.without_url())))?;
        let status = response.status();
        if !status.is_success() {
            return Err(external_http_error(response).await);
        }
        let content_type = content_type(&response);
        let (body, truncated) = read_limited(&mut response, self.max_response_bytes).await?;
        if truncated {
            return Err(ApiError::ResponseTooLarge {
                limit: self.max_response_bytes,
            });
        }
        Ok(ExternalResponse {
            status: status.as_u16(),
            content_type,
            body,
        })
    }

    /// Скачивает файл по подписанной ссылке в `save_to` (правила — как у `save_to`
    /// исполнителя: абсолютный путь, канонический каталог, без перезаписи). Возвращает
    /// число записанных байт.
    pub async fn download_to(&self, url: &str, save_to: &Path) -> Result<u64, ApiError> {
        let url = self.check_url(url)?;
        check_save_to(save_to).await?;
        let response = self
            .request(HttpMethod::Get, url, &[])?
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_reqwest_error(&e.without_url())))?;
        if !response.status().is_success() {
            return Err(external_http_error(response).await);
        }
        save_to_file(response, save_to).await
    }
}

async fn external_http_error(mut response: reqwest::Response) -> ApiError {
    let status = response.status().as_u16();
    let body = match read_limited(&mut response, EXTERNAL_ERROR_BODY_LIMIT).await {
        Ok((bytes, _)) => lossy_prefix(&bytes).trim().to_owned(),
        Err(_) => String::new(),
    };
    ApiError::Http(Box::new(HttpError {
        status,
        errors: Vec::new(),
        body,
        retry_after: None,
        rate_limit: None,
    }))
}
