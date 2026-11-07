//! Тесты исполнителя запросов и JWT на `wiremock`, без реальной сети.
//!
//! Ключи ES256 генерируются в каждом тесте; Key ID и Issuer ID — фиктивные.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use appstore_connect_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, HttpError, Pagination, RateLimit, ResponseBody,
    RetryPolicy,
};
use appstore_connect_api::external::{ExternalClient, ExternalRequest};
use appstore_connect_api::jwt::{AUDIENCE, ManualClock};
use appstore_connect_api::{
    ApiKey, HttpMethod, Issuer, IssuerId, JwtTokens, KeyId, PrivateKeyPem, ResponseKind,
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::elliptic_curve::Generate;
use p256::pkcs8::{EncodePrivateKey, LineEnding};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEY_ID: &str = "TESTKEY123";
const ISSUER: &str = "01234567-89ab-cdef-0123-456789abcdef";
/// Начало отсчёта управляемых часов.
const START: u64 = 1_800_000_000;

struct TestKey {
    signing: SigningKey,
    pem: PrivateKeyPem,
}

fn test_key() -> TestKey {
    let signing = SigningKey::generate();
    let pem = signing.to_pkcs8_pem(LineEnding::LF).unwrap();
    TestKey {
        pem: PrivateKeyPem::parse(&pem).unwrap(),
        signing,
    }
}

fn api_key(key: &TestKey, issuer: Issuer) -> ApiKey {
    ApiKey::new(&key.pem, KeyId::new(KEY_ID).unwrap(), issuer).unwrap()
}

fn team() -> Issuer {
    Issuer::Team(IssuerId::new(ISSUER).unwrap())
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_secs(START)))
}

fn config(base: &str, tokens: Arc<JwtTokens>) -> ClientConfig {
    ClientConfig {
        base_url: url::Url::parse(base).unwrap(),
        tokens: Some(tokens),
        timeout: Duration::from_secs(5),
        max_response_bytes: 64 * 1024,
        retry: RetryPolicy {
            max_retries: 2,
            default_delay: Duration::from_millis(10),
            max_delay: Duration::from_secs(5),
        },
    }
}

/// Клиент с ключом, сгенерированным в тесте, и управляемыми часами.
struct Setup {
    key: TestKey,
    clock: Arc<ManualClock>,
    tokens: Arc<JwtTokens>,
    client: ApiClient,
}

fn setup(server: &MockServer) -> Setup {
    let key = test_key();
    let clock = clock();
    let tokens = Arc::new(JwtTokens::with_clock(api_key(&key, team()), clock.clone()));
    let client = ApiClient::new(config(&server.uri(), tokens.clone())).unwrap();
    Setup {
        key,
        clock,
        tokens,
        client,
    }
}

fn get(path: &str) -> ApiRequest {
    ApiRequest::new(HttpMethod::Get, path)
}

fn http_error(err: ApiError) -> HttpError {
    match err {
        ApiError::Http(e) => *e,
        other => panic!("expected Http, got {other:?}"),
    }
}

fn bearer(request: &Request) -> String {
    let value = request
        .headers
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap();
    value.strip_prefix("Bearer ").unwrap().to_owned()
}

/// Проверяет подпись JWT открытым ключом тестового ключа и возвращает (header, claims).
fn verify_jwt(token: &str, signing: &SigningKey) -> (Value, Value) {
    let parts: Vec<&str> = token.split('.').collect();
    assert_eq!(parts.len(), 3, "{token}");
    let decode = |part: &str| URL_SAFE_NO_PAD.decode(part).unwrap();
    let signature = Signature::from_slice(&decode(parts[2])).unwrap();
    let verifying = VerifyingKey::from(signing);
    verifying
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .expect("JWT signature must verify with the public key");
    // Подпись другим ключом не проходит.
    let other = VerifyingKey::from(&SigningKey::generate());
    assert!(
        other
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .is_err()
    );
    (
        serde_json::from_slice(&decode(parts[0])).unwrap(),
        serde_json::from_slice(&decode(parts[1])).unwrap(),
    )
}

#[tokio::test]
async fn sends_signed_es256_jwt_for_team_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/apps"))
        .and(header(
            "user-agent",
            concat!("appstore-connect-mcp/", env!("CARGO_PKG_VERSION")),
        ))
        .respond_with(|req: &Request| {
            assert_eq!(
                req.headers.get("accept").unwrap(),
                "application/json, */*;q=0.8"
            );
            ResponseTemplate::new(200).set_body_json(json!({"data": []}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let s = setup(&server);
    let response = s.client.execute(get("/v1/apps")).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, ResponseBody::Json(json!({"data": []})));
    assert!(response.pagination.is_empty());
    assert_eq!(response.rate_limit, None);

    let requests = server.received_requests().await.unwrap();
    let (header, claims) = verify_jwt(&bearer(&requests[0]), &s.key.signing);
    assert_eq!(header, json!({"alg": "ES256", "kid": KEY_ID, "typ": "JWT"}));
    assert_eq!(
        claims,
        json!({"iss": ISSUER, "iat": START, "exp": START + 15 * 60, "aud": AUDIENCE})
    );
}

#[tokio::test]
async fn individual_key_uses_sub_user_without_iss() {
    let key = test_key();
    let tokens = JwtTokens::with_clock(api_key(&key, Issuer::Individual), clock());
    let jwt = tokens.token().unwrap();
    let (header, claims) = verify_jwt(jwt.expose(), &key.signing);
    assert_eq!(header["kid"], KEY_ID);
    assert_eq!(
        claims,
        json!({"sub": "user", "iat": START, "exp": START + 15 * 60, "aud": AUDIENCE})
    );
    let exp = claims["exp"].as_u64().unwrap();
    assert!(exp - claims["iat"].as_u64().unwrap() <= 20 * 60);
}

#[tokio::test]
async fn token_is_cached_and_refreshed_before_expiry() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/apps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&server)
        .await;
    let s = setup(&server);

    s.client.execute(get("/v1/apps")).await.unwrap();
    // Через 9 минут до exp больше 5 минут: тот же токен.
    s.clock.advance(Duration::from_secs(9 * 60));
    s.client.execute(get("/v1/apps")).await.unwrap();
    // Через 10 минут 30 секунд до exp меньше 5 минут: новый токен.
    s.clock.advance(Duration::from_secs(90));
    s.client.execute(get("/v1/apps")).await.unwrap();
    // Часы отвели назад раньше iat — тоже новый токен.
    s.clock.set(UNIX_EPOCH + Duration::from_secs(START - 3600));
    s.client.execute(get("/v1/apps")).await.unwrap();

    let tokens: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(bearer)
        .collect();
    assert_eq!(tokens[0], tokens[1]);
    assert_ne!(tokens[1], tokens[2]);
    assert_ne!(tokens[2], tokens[3]);
    let (_, refreshed) = verify_jwt(&tokens[2], &s.key.signing);
    assert_eq!(refreshed["iat"], START + 630);
    assert_eq!(refreshed["exp"], START + 630 + 900);
    let (_, back) = verify_jwt(&tokens[3], &s.key.signing);
    assert_eq!(back["iat"], START - 3600);
    // Источник помнит прежние токены для вырезания из текстов.
    assert!(s.tokens.known_tokens().len() >= 3);
}

#[tokio::test]
async fn missing_key_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let mut cfg = config(&server.uri(), setup(&server).tokens);
    cfg.tokens = None;
    let err = ApiClient::new(cfg)
        .unwrap()
        .execute(get("/v1/apps"))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::MissingKey), "{err:?}");
}

#[tokio::test]
async fn encodes_json_api_query_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/apps"))
        .and(query_param("fields[apps]", "name,bundleId"))
        .and(query_param("include", "builds,appInfos"))
        .and(query_param("filter[name]", "a b&c=d+e"))
        .and(query_param("limit", "1"))
        .respond_with(|req: &Request| {
            // Скобки и запятые JSON:API не кодируются, разделители внутри значений — да.
            let query = req.url.query().unwrap_or_default().to_owned();
            assert!(query.contains("fields[apps]=name,bundleId"), "{query}");
            assert!(query.contains("filter[name]=a%20b%26c%3Dd%2Be"), "{query}");
            ResponseTemplate::new(200).set_body_json(json!({"data": []}))
        })
        .expect(1)
        .mount(&server)
        .await;
    let s = setup(&server);
    let request = get("/v1/apps")
        .with_query("fields[apps]", "name,bundleId")
        .with_query("include", "builds,appInfos")
        .with_query("filter[name]", "a b&c=d+e")
        .with_query("limit", "1");
    s.client.execute(request).await.unwrap();
}

#[tokio::test]
async fn rejects_absolute_and_unsafe_paths_without_sending() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let s = setup(&server);
    for bad in [
        "https://api.appstoreconnect.apple.com/v1/apps",
        "//evil.example/v1/apps",
        "/v1/apps/../../admin",
        "/v1/apps/%2e%2e/x",
        "/v1/apps?limit=1",
        "/apps",
    ] {
        let err = s.client.execute(get(bad)).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidPath(_)), "{bad}: {err:?}");
    }
}

#[tokio::test]
async fn sends_json_body() {
    let server = MockServer::start().await;
    let body =
        json!({"data": {"type": "appStoreVersions", "attributes": {"versionString": "1.2"}}});
    Mock::given(method("POST"))
        .and(path("/v1/appStoreVersions"))
        .and(header("content-type", "application/json"))
        .and(wiremock::matchers::body_json(&body))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"id": "1"}})))
        .expect(2)
        .mount(&server)
        .await;
    let s = setup(&server);
    let request = ApiRequest::new(HttpMethod::Post, "/v1/appStoreVersions").with_json(body.clone());
    assert_eq!(s.client.execute(request).await.unwrap().status, 201);
    // Тело строкой с JSON отправляется как объект.
    let request = ApiRequest::new(HttpMethod::Post, "/v1/appStoreVersions")
        .with_json(Value::String(body.to_string()));
    s.client.execute(request).await.unwrap();
}

#[tokio::test]
async fn retries_429_and_503_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/apps"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/v1/apps"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/v1/apps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(1)
        .mount(&server)
        .await;
    let s = setup(&server);
    let response = s.client.execute(get("/v1/apps")).await.unwrap();
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn retries_are_bounded_and_report_retry_after() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/apps"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .insert_header("x-rate-limit", "user-hour-lim:3600;user-hour-rem:0;")
                .set_body_json(json!({"errors": [{
                    "status": "429",
                    "code": "RATE_LIMIT_EXCEEDED",
                    "title": "The request rate limit has been reached.",
                    "detail": "We've received too many requests for this API."
                }]})),
        )
        // Первая попытка и два повтора.
        .expect(3)
        .mount(&server)
        .await;
    let s = setup(&server);
    let err = http_error(s.client.execute(get("/v1/apps")).await.unwrap_err());
    assert_eq!(err.status, 429);
    assert_eq!(err.retry_after, Some(0));
    assert_eq!(err.errors[0].code.as_deref(), Some("RATE_LIMIT_EXCEEDED"));
    assert_eq!(
        err.rate_limit,
        Some(RateLimit {
            limit: Some(3600),
            remaining: Some(0)
        })
    );
    let text = err.to_string();
    assert!(text.contains("HTTP 429 Too Many Requests"), "{text}");
    assert!(text.contains("0 of 3600 requests left"), "{text}");

    // Слишком долгий Retry-After: без повтора, ошибка сразу.
    let server = MockServer::start().await;
    Mock::given(path("/v1/apps"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
        .expect(1)
        .mount(&server)
        .await;
    let s = setup(&server);
    let started = Instant::now();
    let err = http_error(s.client.execute(get("/v1/apps")).await.unwrap_err());
    assert_eq!(err.retry_after, Some(3600));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn server_errors_are_not_retried_for_post() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/betaGroups"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let s = setup(&server);
    let request = ApiRequest::new(HttpMethod::Post, "/v1/betaGroups").with_json(json!({}));
    let err = http_error(s.client.execute(request).await.unwrap_err());
    assert_eq!(err.status, 503);

    // 429 повторяется и для POST: запрос отклонён до выполнения.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/betaGroups"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/betaGroups"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    let s = setup(&server);
    let request = ApiRequest::new(HttpMethod::Post, "/v1/betaGroups").with_json(json!({}));
    let response = s.client.execute(request).await.unwrap();
    assert_eq!(response.status, 201);
    assert_eq!(response.body, ResponseBody::Empty);
}

#[tokio::test]
async fn formats_json_api_errors() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/apps/123"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"errors": [{
            "id": "6f1d0f34-0000-0000-0000-000000000000",
            "status": "404",
            "code": "NOT_FOUND",
            "title": "The specified resource does not exist",
            "detail": "There is no resource of type 'apps' with id '123'",
            "source": {"parameter": "id"}
        }]})))
        .mount(&server)
        .await;
    let s = setup(&server);
    let err = s.client.execute(get("/v1/apps/123")).await.unwrap_err();
    assert_eq!(err.status(), Some(404));
    let text = err.to_string();
    assert_eq!(
        text,
        "App Store Connect API returned HTTP 404 Not Found: NOT_FOUND: The specified resource \
         does not exist — There is no resource of type 'apps' with id '123' (parameter `id`)"
    );
    let err = http_error(err);
    assert_eq!(
        err.errors[0].id.as_deref(),
        Some("6f1d0f34-0000-0000-0000-000000000000")
    );

    // Ответ без JSON:API — тело как есть.
    let server = MockServer::start().await;
    Mock::given(path("/v1/apps"))
        .respond_with(ResponseTemplate::new(400).set_body_string("plain failure"))
        .mount(&server)
        .await;
    let s = setup(&server);
    let err = s.client.execute(get("/v1/apps")).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "App Store Connect API returned HTTP 400 Bad Request: plain failure"
    );
}

#[tokio::test]
async fn jwt_and_key_never_appear_in_errors_or_debug() {
    let server = MockServer::start().await;
    // Сервер отражает заголовок авторизации в ошибке.
    Mock::given(path("/v1/apps"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(401).set_body_json(json!({"errors": [{
                "status": "401",
                "code": "NOT_AUTHORIZED",
                "title": "Authentication credentials are missing or invalid.",
                "detail": format!("Rejected header: {auth}"),
            }]}))
        })
        .mount(&server)
        .await;
    Mock::given(path("/v1/echo"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/plain")
                .set_body_string(format!("you sent {auth}"))
        })
        .mount(&server)
        .await;
    let s = setup(&server);
    let jwt = s.tokens.token().unwrap();
    let jwt = jwt.expose().to_owned();

    let err = s.client.execute(get("/v1/apps")).await.unwrap_err();
    let texts = [err.to_string(), format!("{err:?}")];
    for text in &texts {
        assert!(!text.contains(&jwt), "{text}");
        assert!(text.contains("Rejected header: Bearer ***"), "{text}");
    }
    let response = s.client.execute(get("/v1/echo")).await.unwrap();
    assert_eq!(
        response.body,
        ResponseBody::Text("you sent Bearer ***".into())
    );
    assert_eq!(s.client.scrub_token(format!("x {jwt} y")), "x *** y");

    let pem = s.key.pem.expose().to_owned();
    let pem_body: String = pem.lines().nth(1).unwrap().to_owned();
    for debug in [
        format!("{:?}", s.client),
        format!("{:?}", s.client.config()),
        format!("{:?}", s.tokens),
        format!("{:?}", s.key.pem),
        format!("{:?}", s.tokens.token().unwrap()),
    ] {
        assert!(!debug.contains(&jwt), "{debug}");
        assert!(!debug.contains(&pem_body), "{debug}");
        assert!(!debug.contains(KEY_ID), "{debug}");
        assert!(!debug.contains(ISSUER), "{debug}");
    }
}

#[tokio::test]
async fn pagination_follows_links_next_on_the_api_host() {
    let server = MockServer::start().await;
    let next = format!(
        "{}/v1/apps?cursor=Mg.ANu1gHA&limit=2&fields[apps]=name,bundleId",
        server.uri()
    );
    Mock::given(path("/v1/apps"))
        .and(query_param("cursor", "Mg.ANu1gHA"))
        .and(query_param("fields[apps]", "name,bundleId"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "3"}],
            "links": {"self": format!("{}/v1/apps", server.uri())},
            "meta": {"paging": {"total": 3, "limit": 2}}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/v1/apps"))
        .and(query_param("limit", "2"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-rate-limit", "user-hour-lim:3600;user-hour-rem:3598;")
                .set_body_json(json!({
                    "data": [{"id": "1"}, {"id": "2"}],
                    "links": {"self": format!("{}/v1/apps", server.uri()), "next": next},
                    "meta": {"paging": {"total": 3, "limit": 2}}
                })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let s = setup(&server);

    let first = s
        .client
        .execute(get("/v1/apps").with_query("limit", "2"))
        .await
        .unwrap();
    assert_eq!(
        first.pagination,
        Pagination {
            next: Some(next.clone()),
            total: Some(3),
            limit: Some(2),
        }
    );
    assert_eq!(
        first.rate_limit,
        Some(RateLimit {
            limit: Some(3600),
            remaining: Some(3598)
        })
    );
    let request = s.client.next_page_request(&next).unwrap();
    assert_eq!(request.method, HttpMethod::Get);
    assert_eq!(request.path, "/v1/apps");
    assert!(
        request
            .query
            .contains(&("fields[apps]".to_owned(), "name,bundleId".to_owned()))
    );

    let second = s.client.execute_next(&next).await.unwrap();
    assert_eq!(second.pagination.next, None);
    assert_eq!(second.pagination.total, Some(3));
}

#[tokio::test]
async fn next_page_urls_from_other_hosts_are_rejected() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let s = setup(&server);
    let port = url::Url::parse(&server.uri()).unwrap().port().unwrap();
    for bad in [
        "https://evil.example/v1/apps?cursor=x".to_owned(),
        "https://api.appstoreconnect.apple.com/v1/apps?cursor=x".to_owned(),
        format!("http://localhost.evil.example:{port}/v1/apps"),
        format!("http://127.0.0.1:{}/v1/apps", port.wrapping_add(1)),
        format!("https://127.0.0.1:{port}/v1/apps"),
        format!("http://user:pass@127.0.0.1:{port}/v1/apps"),
        format!("http://127.0.0.1:{port}/v1/apps/../../admin"),
        format!("http://127.0.0.1:{port}/admin"),
        "/v1/apps?cursor=x".to_owned(),
        "not a url".to_owned(),
    ] {
        let err = s.client.execute_next(&bad).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidNextUrl(_)), "{bad}: {err:?}");
        assert!(
            err.to_string().contains(appstore_connect_api::API_HOST),
            "{err}"
        );
    }

    // Ссылка на чужой хост в ответе не попадает в выдачу.
    let server = MockServer::start().await;
    Mock::given(path("/v1/apps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "links": {"next": "https://evil.example/v1/apps?cursor=x"},
            "meta": {"paging": {"total": 5, "limit": 1}}
        })))
        .mount(&server)
        .await;
    let s = setup(&server);
    let response = s.client.execute(get("/v1/apps")).await.unwrap();
    assert_eq!(response.pagination.next, None);
    assert_eq!(response.pagination.total, Some(5));
}

#[tokio::test]
async fn production_client_accepts_only_the_apple_api_host() {
    let key = test_key();
    let tokens = Arc::new(JwtTokens::new(api_key(&key, team())));
    let client = ApiClient::new(ClientConfig::new(Some(tokens)).unwrap()).unwrap();
    let request = client
        .next_page_request("https://api.appstoreconnect.apple.com/v1/apps?cursor=AB&limit=2")
        .unwrap();
    assert_eq!(request.path, "/v1/apps");
    for bad in [
        "http://api.appstoreconnect.apple.com/v1/apps",
        "https://api.appstoreconnect.apple.com.evil.example/v1/apps",
        "https://evil.example/v1/apps",
        "https://api.appstoreconnect.apple.com:8443/v1/apps",
    ] {
        assert!(
            matches!(
                client.next_page_request(bad),
                Err(ApiError::InvalidNextUrl(_))
            ),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn binary_reports_are_returned_as_bytes_or_saved() {
    let gzip: Vec<u8> = vec![0x1f, 0x8b, 0x08, 0x00, 0xff, 0x00, 0x10];
    let server = MockServer::start().await;
    let body = gzip.clone();
    Mock::given(path("/v1/salesReports"))
        .respond_with(move |req: &Request| {
            assert_eq!(
                req.headers.get("accept").unwrap(),
                "application/a-gzip, application/octet-stream;q=0.9, */*;q=0.8"
            );
            ResponseTemplate::new(200).set_body_raw(body.clone(), "application/a-gzip")
        })
        .expect(2)
        .mount(&server)
        .await;
    let s = setup(&server);
    let mut request = get("/v1/salesReports").with_query("filter[frequency]", "DAILY");
    request.response = ResponseKind::Binary;

    let response = s.client.execute(request.clone()).await.unwrap();
    assert_eq!(
        response.body,
        ResponseBody::Binary {
            bytes: gzip.clone(),
            content_type: Some("application/a-gzip".into()),
        }
    );

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().canonicalize().unwrap().join("report.tsv.gz");
    request.save_to = Some(target.clone());
    let response = s.client.execute(request.clone()).await.unwrap();
    assert_eq!(
        response.body,
        ResponseBody::SavedToFile {
            path: target.clone(),
            bytes: gzip.len() as u64,
            content_type: Some("application/a-gzip".into()),
        }
    );
    assert_eq!(std::fs::read(&target).unwrap(), gzip);

    // Существующий файл не перезаписывается, запрос не отправляется (expect(2) выше).
    let err = s.client.execute(request).await.unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[tokio::test]
async fn large_responses_are_rejected() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/apps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": "x".repeat(2000)})))
        .mount(&server)
        .await;
    let s = setup(&server);
    let err = s
        .client
        .execute_with_limit(get("/v1/apps"), 1000)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ApiError::ResponseTooLarge { limit: 1000 }),
        "{err:?}"
    );
    // Лимит конфигурации (64 КиБ) пропускает тот же ответ.
    s.client.execute(get("/v1/apps")).await.unwrap();
}

#[tokio::test]
async fn redirects_to_other_hosts_are_not_followed() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/apps"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://evil.example/steal"),
        )
        .mount(&server)
        .await;
    let s = setup(&server);
    let err = http_error(s.client.execute(get("/v1/apps")).await.unwrap_err());
    assert_eq!(err.status, 302);
    assert!(!err.body.contains("evil.example"), "{}", err.body);
}

#[tokio::test]
async fn external_uploads_carry_no_authorization() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/upload/part-1"))
        .and(header("content-type", "image/png"))
        .respond_with(|req: &Request| {
            assert!(req.headers.get("authorization").is_none());
            assert_eq!(req.body, b"PNGDATA");
            ResponseTemplate::new(200)
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/segment.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1, 2, 3], "application/gzip"))
        .expect(1)
        .mount(&server)
        .await;

    let client = ExternalClient::new(Duration::from_secs(5), 1024)
        .unwrap()
        .allow_loopback_http();
    let upload = ExternalRequest {
        method: HttpMethod::Put,
        url: format!("{}/upload/part-1?X-Amz-Signature=secret", server.uri()),
        headers: vec![("Content-Type".into(), "image/png".into())],
        body: b"PNGDATA".to_vec(),
    };
    assert!(!format!("{upload:?}").contains("secret"));
    let response = client.send(upload.clone()).await.unwrap();
    assert_eq!(response.status, 200);

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().canonicalize().unwrap().join("segment.gz");
    let written = client
        .download_to(&format!("{}/segment.gz", server.uri()), &target)
        .await
        .unwrap();
    assert_eq!(written, 3);

    // Заголовок авторизации задать нельзя; http — только для loopback и только если разрешено.
    let mut with_auth = upload.clone();
    with_auth
        .headers
        .push(("Authorization".into(), "Bearer x".into()));
    let err = client.send(with_auth).await.unwrap_err();
    assert!(err.to_string().contains("authorization"), "{err}");
    let strict = ExternalClient::new(Duration::from_secs(5), 1024).unwrap();
    let err = strict.send(upload.clone()).await.unwrap_err();
    assert!(err.to_string().contains("only https"), "{err}");
    assert!(!err.to_string().contains("secret"), "{err}");
    let mut remote_http = upload;
    remote_http.url = "http://uploads.example/part".into();
    assert!(client.send(remote_http).await.is_err());
}

#[test]
fn system_time_is_used_by_default() {
    let key = test_key();
    let tokens = JwtTokens::new(api_key(&key, team()));
    let jwt = tokens.token().unwrap();
    let (_, claims) = verify_jwt(jwt.expose(), &key.signing);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let iat = claims["iat"].as_u64().unwrap();
    assert!(iat <= now && now - iat < 60, "{iat} vs {now}");
}
