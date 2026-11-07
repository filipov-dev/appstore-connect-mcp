//! Вырезание учётных данных через сервер: ответы инструментов операций и `api_request`,
//! ошибки, фильтр `jq` после вырезания, адреса загрузки и скачивания, содержимое профилей в
//! ответе и в файле `save_to`. App Store Connect API подменён `wiremock`.

mod common;

use std::sync::Arc;

use appstore_connect_api::AccessLevel;
use appstore_connect_mcp_core::composite::testing::MemoryVault;
use appstore_connect_mcp_core::{FilePolicy, ServerOptions};
use common::{args, json_of, server, server_with, text};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PASSWORD: &str = "hunter2-review-password";
/// Форма JWT, значения не настоящие.
const JWT: &str = concat!(
    "eyJhbGciOiJFUzI1NiIsImtpZCI6IlRFU1QifQ",
    ".",
    "eyJpc3MiOiJ0ZXN0IiwiYXVkIjoiYXBwc3RvcmVjb25uZWN0LXYxIn0",
    ".",
    "c2lnbmF0dXJlc2lnbmF0dXJl"
);

fn options() -> ServerOptions {
    ServerOptions {
        max_access: AccessLevel::Read,
        toolsets: Some(vec!["all".into()]),
        composite_tools: Vec::new(),
        ..ServerOptions::default()
    }
}

async fn review_detail(api: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/appStoreReviewDetails/r1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"type": "appStoreReviewDetails", "id": "r1", "attributes": {
                "demoAccountName": "reviewer@example.com",
                "demoAccountPassword": PASSWORD,
                "demoAccountRequired": true,
                "notes": format!("token {JWT}")
            }}
        })))
        .mount(api)
        .await;
}

/// Главный сценарий: пароль демо-аккаунта и JWT в тексте скрыты, пометка — в обёртке.
#[tokio::test]
async fn review_passwords_are_hidden() {
    let api = MockServer::start().await;
    review_detail(&api).await;
    let server = server(&api.uri(), options());
    let result = server
        .call("get_app_store_review_detail", args(json!({"id": "r1"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = text(&result);
    assert!(!body.contains(PASSWORD) && !body.contains(JWT), "{body}");
    let value = json_of(&result);
    let attributes = &value["result"]["data"]["attributes"];
    assert_eq!(attributes["demoAccountPassword"], "[redacted]");
    assert_eq!(attributes["demoAccountName"], "reviewer@example.com");
    assert_eq!(attributes["notes"], "token [redacted]");
    let note = value["note"].as_str().unwrap();
    assert!(note.contains("demoAccountPassword"), "{note}");
}

/// Фильтр видит только очищенный ответ: ни поиском строк, ни преобразованием нельзя достать
/// вырезанные значения.
#[tokio::test]
async fn filter_sees_only_redacted_values() {
    let api = MockServer::start().await;
    review_detail(&api).await;
    let server = server(&api.uri(), options());
    for filter in [
        "[.. | strings]",
        ".data.attributes.demoAccountPassword | @base64",
        ".data.attributes | to_entries",
    ] {
        let result = server
            .call(
                "get_app_store_review_detail",
                args(json!({"id": "r1", "jq": filter})),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{filter}: {}", text(&result));
        let body = text(&result);
        assert!(!body.contains(PASSWORD), "{filter}: {body}");
        assert!(!body.contains("aHVudGVy"), "{filter}: {body}");
        assert!(!body.contains(JWT), "{filter}: {body}");
        assert!(json_of(&result)["note"].is_string(), "{filter}: {body}");
    }
}

#[tokio::test]
async fn upload_and_report_urls_are_hidden() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/appScreenshots/s1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"type": "appScreenshots", "id": "s1", "attributes": {
                "fileName": "shot.png",
                "uploadOperations": [{
                    "method": "PUT",
                    "url": "https://upload.example.com/part1?X-Amz-Signature=abc",
                    "length": 10,
                    "offset": 0,
                    "requestHeaders": [{"name": "Content-Type", "value": "image/png"}]
                }]
            }}
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/analyticsReportInstances/i1/segments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"type": "analyticsReportSegments", "id": "g1", "attributes": {
                "checksum": "c", "sizeInBytes": 5, "url": "https://reports.example.com/g1.gz"
            }}]
        })))
        .mount(&api)
        .await;
    let server = server(&api.uri(), options());

    let result = server
        .call("get_app_screenshot", args(json!({"id": "s1"})))
        .await
        .unwrap();
    let value = json_of(&result);
    let operation = &value["result"]["data"]["attributes"]["uploadOperations"][0];
    assert_eq!(operation["url"], "[redacted]");
    assert_eq!(operation["requestHeaders"][0]["value"], "image/png");
    assert!(value["note"].as_str().unwrap().contains("`asset_upload`"));

    let result = server
        .call(
            "list_analytics_report_instance_segments",
            args(json!({"id": "i1"})),
        )
        .await
        .unwrap();
    let value = json_of(&result);
    assert_eq!(
        value["result"]["data"][0]["attributes"]["url"],
        "[redacted]"
    );
    assert!(
        value["note"]
            .as_str()
            .unwrap()
            .contains("`report_download`")
    );
}

/// Содержимое профиля в ответе агенту заменено пояснением, а в файле `save_to` остаётся;
/// учётные данные не попадают и в файл.
#[tokio::test]
async fn profile_content_is_omitted_but_saved_to_files() {
    let content = "MIIQ".to_owned() + &"A".repeat(4000);
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/profiles/p1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"type": "profiles", "id": "p1", "attributes": {
                "name": "Dev", "profileContent": content, "password": PASSWORD
            }}
        })))
        .mount(&api)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let server = server_with(
        &api.uri(),
        options(),
        FilePolicy::new(std::slice::from_ref(&root)).unwrap(),
        Arc::new(MemoryVault::default()),
    );

    let result = server
        .call("get_profile", args(json!({"id": "p1"})))
        .await
        .unwrap();
    let value = json_of(&result);
    let shown = value["result"]["data"]["attributes"]["profileContent"]
        .as_str()
        .unwrap();
    assert!(shown.starts_with("[omitted: 4004 characters"), "{shown}");
    assert!(
        value["note"]
            .as_str()
            .unwrap()
            .contains("`profile_download`")
    );

    let target = root.join("profile.json");
    let result = server
        .call(
            "api_request",
            args(json!({"path": "/v1/profiles/p1", "save_to": target.display().to_string()})),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let saved = std::fs::read_to_string(&target).unwrap();
    assert!(saved.contains(&content));
    assert!(!saved.contains(PASSWORD), "{saved}");
    assert!(json_of(&result)["note"].is_string(), "{}", text(&result));
}

#[tokio::test]
async fn errors_are_redacted() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/apps/1"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "errors": [{
                "status": "409",
                "code": "ENTITY_ERROR",
                "title": "Conflict",
                "detail": format!("Authorization: Bearer {JWT} rejected; password {PASSWORD}")
            }]
        })))
        .mount(&api)
        .await;
    let server = server(&api.uri(), options());
    let result = server
        .call("get_app", args(json!({"id": "1"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(!message.contains(JWT), "{message}");
    assert!(message.contains("Bearer [redacted]"), "{message}");
    assert!(message.contains("do not ask the user"), "{message}");
}
