//! Фильтр `jq` через сервер: инструменты операций с постраничной выдачей и `flatten`,
//! `api_request`, составные инструменты, ошибки до и после запроса, лимит размера.
//! App Store Connect API подменён `wiremock`.

mod common;

use std::sync::Arc;

use appstore_connect_api::AccessLevel;
use appstore_connect_mcp_core::composite::testing::MemoryVault;
use appstore_connect_mcp_core::composite::{CompositeTool, Context, ToolFuture};
use appstore_connect_mcp_core::{FilePolicy, ServerOptions};
use common::{args, json_of, server, server_with, text};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn builds(next: Option<&str>) -> Value {
    let mut body = json!({
        "data": [
            {"type": "builds", "id": "b1", "attributes": {"version": "41", "processingState": "VALID"},
             "relationships": {"app": {"data": {"type": "apps", "id": "a1"}}}},
            {"type": "builds", "id": "b2", "attributes": {"version": "42", "processingState": "PROCESSING"},
             "relationships": {"app": {"data": {"type": "apps", "id": "a1"}}}}
        ],
        "included": [{"type": "apps", "id": "a1", "attributes": {"name": "Demo"}}],
        "links": {"self": "x"},
        "meta": {"paging": {"total": 4, "limit": 2}}
    });
    if let Some(next) = next {
        body["links"]["next"] = json!(next);
    }
    body
}

fn run_listing(_ctx: &Context, _args: Value) -> ToolFuture<'_> {
    Box::pin(async { Ok(json!({"items": [{"id": 1, "secret": false}, {"id": 2}]})) })
}

fn listing_tool() -> CompositeTool {
    CompositeTool {
        name: "test_listing",
        toolset: "apps",
        access: AccessLevel::Read,
        description: "Test composite tool.",
        input_schema: || json!({"type": "object", "additionalProperties": false, "properties": {}}),
        uses_local_files: false,
        run: run_listing,
    }
}

fn options() -> ServerOptions {
    ServerOptions {
        max_access: AccessLevel::Write,
        composite_tools: vec![listing_tool()],
        ..ServerOptions::default()
    }
}

/// Главный сценарий: фильтр списка сборок возвращает только нужные поля, а постраничная
/// выдача остаётся рядом с результатом; с `flatten` фильтр видит плоские объекты.
#[tokio::test]
async fn filters_lists_and_keeps_pagination() {
    let api = MockServer::start().await;
    let next = format!("{}/v1/builds?cursor=AB", api.uri());
    Mock::given(method("GET"))
        .and(path("/v1/builds"))
        .respond_with(ResponseTemplate::new(200).set_body_json(builds(Some(&next))))
        .expect(2)
        .mount(&api)
        .await;
    let server = server(&api.uri(), options());

    let result = server
        .call(
            "list_builds",
            args(json!({"limit": 2, "jq": "[.data[] | {id, version: .attributes.version}]"})),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value = json_of(&result);
    assert_eq!(
        value["result"],
        json!([{"id": "b1", "version": "41"}, {"id": "b2", "version": "42"}])
    );
    assert_eq!(value["pagination"]["next"], next);
    assert_eq!(value["pagination"]["total"], 4);

    let result = server
        .call(
            "list_builds",
            args(json!({
                "include": ["app"],
                "flatten": true,
                "jq": "[.data[] | {version, app: .app.name}]"
            })),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(
        json_of(&result)["result"],
        json!([{"version": "41", "app": "Demo"}, {"version": "42", "app": "Demo"}])
    );
}

#[tokio::test]
async fn filters_api_request_and_composite_tools() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/builds"))
        .respond_with(ResponseTemplate::new(200).set_body_json(builds(None)))
        .expect(1)
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
        .call(
            "api_request",
            args(json!({"path": "/v1/builds", "jq": "[.data[] | select(.attributes.processingState == \"VALID\") | .id]"})),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    // `meta.paging` остаётся в обёртке рядом с результатом фильтра.
    let value = json_of(&result);
    assert_eq!(value["result"], json!(["b1"]));
    assert_eq!(value["pagination"]["total"], 4);

    // С `save_to` фильтр не сочетается; запрос не отправляется (`expect(1)` выше).
    let result = server
        .call(
            "api_request",
            args(json!({
                "path": "/v1/builds",
                "jq": ".",
                "save_to": root.join("builds.json").display().to_string()
            })),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("`save_to`"), "{}", text(&result));
    assert!(!root.join("builds.json").exists());

    let result = server
        .call("test_listing", args(json!({"jq": ".items[].id"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    // Несколько результатов — массив.
    assert_eq!(text(&result), "[1,2]");
    let tool = server.registry().get("test_listing").unwrap().tool.clone();
    assert!(tool.input_schema["properties"].get("jq").is_some());
}

/// Сломанный фильтр и фильтр, читающий окружение, отклоняются до запроса к API.
#[tokio::test]
async fn broken_filter_is_reported_before_the_request() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/betaGroups"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"id": "g1"}})))
        .expect(0)
        .mount(&api)
        .await;
    let server = server(&api.uri(), options());
    for (filter, needle) in [
        (json!(".data |"), "syntax"),
        (json!("env.HOME"), "undefined filter"),
        (json!("$ENV"), "undefined"),
        (json!(42), "must be a string"),
        (json!(""), "empty"),
    ] {
        let result = server
            .call(
                "create_beta_group",
                args(json!({"body": {"data": {"type": "betaGroups"}}, "jq": filter})),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{filter}");
        let message = text(&result);
        assert!(message.contains("`jq`"), "{filter}: {message}");
        assert!(message.contains(needle), "{filter}: {message}");
    }
}

/// Ошибка выполнения фильтра после записи: агент узнаёт, что запрос выполнен.
#[tokio::test]
async fn runtime_error_says_the_request_succeeded() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/betaGroups"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"id": "g1"}})))
        .expect(1)
        .mount(&api)
        .await;
    let server = server(&api.uri(), options());
    let result = server
        .call(
            "create_beta_group",
            args(json!({"body": {"data": {"type": "betaGroups"}}, "jq": ".data.id | error"})),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("succeeded (HTTP 201)"), "{message}");
    assert!(message.contains("Do not repeat"), "{message}");
}

/// Лимит ответа действует на результат фильтра: большой ответ можно сузить фильтром, а
/// слишком большой результат — ошибка с подсказкой.
#[tokio::test]
async fn size_limit_applies_to_the_filtered_result() {
    let api = MockServer::start().await;
    let big = "x".repeat(300 * 1024);
    Mock::given(method("GET"))
        .and(path("/v1/apps/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"type": "apps", "id": "1", "attributes": {"name": "Demo", "description": big}}
        })))
        .mount(&api)
        .await;
    let server = server(&api.uri(), options());
    let result = server
        .call("get_app", args(json!({"id": "1"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("`jq`"), "{}", text(&result));

    let result = server
        .call(
            "get_app",
            args(json!({"id": "1", "jq": ".data.attributes.name"})),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#""Demo""#);

    let result = server
        .call(
            "get_app",
            args(json!({"id": "1", "jq": ".data.attributes.description"})),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("filtered result"),
        "{}",
        text(&result)
    );
}

/// Не-JSON ответ возвращается без фильтра, с пояснением.
#[tokio::test]
async fn non_json_response_is_returned_unfiltered() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/apps/1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/plain")
                .set_body_string("plain text"),
        )
        .mount(&api)
        .await;
    let server = server(&api.uri(), options());
    let result = server
        .call("get_app", args(json!({"id": "1", "jq": ".data"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert!(text(&result).contains("plain text"), "{}", text(&result));
    assert!(text(&result).contains("not applied"), "{}", text(&result));
}

/// Операции с двоичным ответом сохраняют файл: аргумента `jq` у них нет.
#[test]
fn binary_downloads_have_no_jq() {
    let server = server("http://127.0.0.1:9", options());
    let registry = server.registry();
    let options = ServerOptions {
        toolsets: Some(vec!["analytics-reports".into()]),
        ..options()
    };
    let reports = appstore_connect_mcp_core::ToolRegistry::new(&options).unwrap();
    let tool = reports.get("list_sales_reports").unwrap();
    assert!(tool.tool.input_schema["properties"].get("jq").is_none());
    assert!(
        tool.tool.input_schema["properties"]
            .get("save_to")
            .is_some()
    );
    let tool = registry.get("list_builds").unwrap();
    assert!(tool.tool.input_schema["properties"].get("jq").is_some());
}
