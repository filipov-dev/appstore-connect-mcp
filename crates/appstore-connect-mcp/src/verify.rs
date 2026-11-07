//! Проверка ключа API запросом `GET /v1/apps?limit=1` через общий исполнитель.
//!
//! JWT выпускается в памяти только на время проверки; ни он, ни ключ не попадают в вывод.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use appstore_connect_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, DEFAULT_BASE_URL, RetryPolicy,
};
use appstore_connect_api::{ApiKey, HttpMethod, JwtTokens};

/// Путь проверочного запроса.
pub const CHECK_PATH: &str = "/v1/apps";

/// Проверочный запрос, как его видит пользователь.
pub const CHECK_REQUEST: &str = "GET /v1/apps?limit=1";

/// Таймаут проверочного запроса.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Результат успешной проверки: только статус и число приложений, без данных кабинета.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub status: u16,
    /// Сколько приложений видит ключ (`meta.paging.total`), если API сообщил.
    pub apps: Option<u64>,
}

impl fmt::Display for CheckOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{CHECK_REQUEST} returned HTTP {}", self.status)?;
        match self.apps {
            Some(1) => write!(f, ", the key sees 1 app"),
            Some(count) => write!(f, ", the key sees {count} apps"),
            None => Ok(()),
        }
    }
}

/// Проверка ключа. В тестах подменяется, чтобы не ходить в сеть.
pub trait KeyVerifier {
    async fn check(&self, key: &ApiKey) -> anyhow::Result<CheckOutcome>;
}

/// Проверка через App Store Connect API.
#[derive(Debug, Clone)]
pub struct ApiVerifier {
    base_url: url::Url,
}

impl ApiVerifier {
    /// Проверка через настоящий API.
    pub fn new() -> anyhow::Result<Self> {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// Проверка через API по другому адресу (тесты).
    pub fn with_base_url(base_url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            base_url: url::Url::parse(base_url).context("invalid API address")?,
        })
    }
}

impl KeyVerifier for ApiVerifier {
    async fn check(&self, key: &ApiKey) -> anyhow::Result<CheckOutcome> {
        let config = ClientConfig {
            base_url: self.base_url.clone(),
            tokens: Some(Arc::new(JwtTokens::new(key.clone()))),
            timeout: CHECK_TIMEOUT,
            max_response_bytes: 256 * 1024,
            retry: RetryPolicy {
                max_retries: 1,
                ..RetryPolicy::default()
            },
        };
        let client = ApiClient::new(config)?;
        let request = ApiRequest::new(HttpMethod::Get, CHECK_PATH).with_query("limit", "1");
        match client.execute(request).await {
            Ok(response) => Ok(CheckOutcome {
                status: response.status,
                apps: response.pagination.total,
            }),
            Err(err) => Err(describe_failure(&err)),
        }
    }
}

/// Понятная ошибка проверки. Тексты исполнителя уже без JWT.
fn describe_failure(err: &ApiError) -> anyhow::Error {
    match err.status() {
        Some(401) => anyhow!(
            "App Store Connect rejected the key ({CHECK_REQUEST}: HTTP 401). Check the Key ID, \
             the Issuer ID (or --individual for an individual key), that the key file belongs \
             to this Key ID and that the key has not been revoked. Details: {err}"
        ),
        Some(403) => anyhow!(
            "App Store Connect denied access ({CHECK_REQUEST}: HTTP 403): the key's role may \
             not allow listing apps. Details: {err}"
        ),
        _ => anyhow!("{CHECK_REQUEST} failed: {err}"),
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// Проверка без сети: принимает только ключ с заданным Key ID.
    #[derive(Debug)]
    pub struct FakeVerifier {
        pub valid_key_id: String,
    }

    impl KeyVerifier for FakeVerifier {
        async fn check(&self, key: &ApiKey) -> anyhow::Result<CheckOutcome> {
            if key.key_id().as_str() == self.valid_key_id {
                Ok(CheckOutcome {
                    status: 200,
                    apps: Some(2),
                })
            } else {
                Err(anyhow!(
                    "App Store Connect rejected the key ({CHECK_REQUEST}: HTTP 401)"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use appstore_connect_api::{Issuer, IssuerId, KeyId, PrivateKeyPem};
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::testing::generated_key_pem;

    fn key() -> ApiKey {
        let pem = PrivateKeyPem::parse(&generated_key_pem()).unwrap();
        ApiKey::new(
            &pem,
            KeyId::new("TESTKEY123").unwrap(),
            Issuer::Team(IssuerId::new("01234567-89ab-cdef-0123-456789abcdef").unwrap()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn checks_with_a_bearer_jwt() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(CHECK_PATH))
            .and(query_param("limit", "1"))
            .respond_with(|req: &Request| {
                let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
                assert!(auth.starts_with("Bearer ey"), "{auth}");
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": [{"type": "apps", "id": "1"}],
                    "meta": {"paging": {"total": 4, "limit": 1}}
                }))
            })
            .expect(1)
            .mount(&server)
            .await;
        let outcome = ApiVerifier::with_base_url(&server.uri())
            .unwrap()
            .check(&key())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            CheckOutcome {
                status: 200,
                apps: Some(4)
            }
        );
        assert_eq!(
            outcome.to_string(),
            "GET /v1/apps?limit=1 returned HTTP 200, the key sees 4 apps"
        );
    }

    #[tokio::test]
    async fn maps_rejections_without_secrets() {
        let server = MockServer::start().await;
        Mock::given(path(CHECK_PATH))
            .respond_with(|req: &Request| {
                let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
                ResponseTemplate::new(401).set_body_json(json!({"errors": [{
                    "status": "401",
                    "code": "NOT_AUTHORIZED",
                    "title": "Authentication credentials are missing or invalid.",
                    "detail": format!("got {auth}")
                }]}))
            })
            .mount(&server)
            .await;
        let err = ApiVerifier::with_base_url(&server.uri())
            .unwrap()
            .check(&key())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("rejected the key"), "{err}");
        assert!(err.contains("NOT_AUTHORIZED"), "{err}");
        assert!(err.contains("got Bearer ***"), "{err}");
        assert!(!err.contains("Bearer ey"), "{err}");
    }

    #[tokio::test]
    async fn transport_errors_are_reported() {
        // Порт 9 (discard) на localhost закрыт: соединение не устанавливается.
        let err = ApiVerifier::with_base_url("http://127.0.0.1:9")
            .unwrap()
            .check(&key())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("could not reach"), "{err}");
    }
}
