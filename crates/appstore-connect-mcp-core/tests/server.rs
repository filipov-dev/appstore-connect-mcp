//! MCP-сессия с сервером ядра в памяти: инструменты операций против `wiremock`, составной
//! инструмент и промпт, написанные для теста.

mod common;

use std::collections::BTreeMap;

use appstore_connect_api::AccessLevel;
use appstore_connect_mcp_core::composite::{CompositeTool, Context, ToolError, ToolFuture};
use appstore_connect_mcp_core::prompts::{PromptArg, PromptDef};
use appstore_connect_mcp_core::{ServerOptions, ToolKind};
use common::{call, connect, json_of, server, text};
use rmcp::model::GetPromptRequestParams;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header_regex, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn run_echo(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        if args["fail"] == true {
            return Err(ToolError::new("echo failed on request"));
        }
        let app = ctx
            .api
            .execute(appstore_connect_api::executor::ApiRequest::new(
                appstore_connect_api::HttpMethod::Get,
                format!("/v1/apps/{}", args["app_id"].as_str().unwrap_or_default()),
            ))
            .await?;
        let name = match app.body {
            appstore_connect_api::executor::ResponseBody::Json(body) => {
                body["data"]["attributes"]["name"].clone()
            }
            _ => Value::Null,
        };
        Ok(json!({
            "name": name,
            "profile": ctx.profile.name,
            "vendor": ctx.profile.vendor_number,
            "access": ctx.access.as_str(),
        }))
    })
}

fn echo_tool() -> CompositeTool {
    CompositeTool {
        name: "test_echo",
        toolset: "apps",
        access: AccessLevel::Read,
        description: "Test composite tool: reads the app name.",
        input_schema: || {
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["app_id"],
                "properties": {
                    "app_id": {"type": "string"},
                    "fail": {"type": "boolean"}
                }
            })
        },
        uses_local_files: false,
        run: run_echo,
    }
}

fn render_test(args: &BTreeMap<String, String>) -> String {
    format!(
        "Check the app {} ({}).",
        args.get("app").map(String::as_str).unwrap_or("?"),
        args.get("platform")
            .map(String::as_str)
            .unwrap_or("any platform")
    )
}

const TEST_PROMPT: PromptDef = PromptDef {
    name: "test_prompt",
    title: "Test prompt",
    description: "A prompt for tests.",
    arguments: &[
        PromptArg {
            name: "app",
            description: "App name or ID.",
            required: true,
        },
        PromptArg {
            name: "platform",
            description: "Platform.",
            required: false,
        },
    ],
    render: render_test,
};

fn options() -> ServerOptions {
    ServerOptions {
        profile: Some("work".into()),
        vendor_number: Some("80000000".into()),
        composite_tools: vec![echo_tool()],
        prompts: vec![TEST_PROMPT],
        ..ServerOptions::default()
    }
}

#[tokio::test]
async fn operation_tools_build_requests_and_wrap_pagination() {
    let api = MockServer::start().await;
    let next = format!("{}/v1/apps?cursor=AB&limit=1", api.uri());
    Mock::given(method("GET"))
        .and(path("/v1/apps"))
        .and(query_param("fields[apps]", "name,bundleId"))
        .and(query_param("filter[bundleId]", "dev.demo"))
        .and(query_param("limit", "1"))
        .and(header_regex("authorization", r"^Bearer eyJ"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-rate-limit", "user-hour-lim:3600;user-hour-rem:20;")
                .set_body_json(json!({
                    "data": [{"type": "apps", "id": "1", "attributes": {"name": "Demo"}}],
                    "links": {"self": "x", "next": next},
                    "meta": {"paging": {"total": 2, "limit": 1}}
                })),
        )
        .expect(1)
        .mount(&api)
        .await;

    let client = connect(server(&api.uri(), options())).await;
    let result = call(
        &client,
        "list_apps",
        json!({"fields.apps": ["name", "bundleId"], "filter.bundleId": ["dev.demo"], "limit": 1}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value = json_of(&result);
    assert_eq!(value["result"]["data"][0]["id"], "1");
    assert_eq!(value["pagination"]["total"], 2);
    assert_eq!(value["pagination"]["next"], next);
    assert_eq!(value["rate_limit"]["remaining"], 20);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn flatten_replaces_references() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/apps/1"))
        .and(query_param("include", "builds"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "type": "apps", "id": "1",
                "attributes": {"name": "Demo"},
                "relationships": {"builds": {"data": [{"type": "builds", "id": "b1"}]}}
            },
            "included": [{"type": "builds", "id": "b1", "attributes": {"version": "7"}}],
            "links": {"self": "x"}
        })))
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options())).await;
    let result = call(
        &client,
        "get_app",
        json!({"id": "1", "include": ["builds"], "flatten": true}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(
        json_of(&result),
        json!({"data": {"id": "1", "type": "apps", "name": "Demo",
            "builds": [{"id": "b1", "type": "builds", "version": "7"}]}})
    );
    let raw = call(
        &client,
        "get_app",
        json!({"id": "1", "include": ["builds"]}),
    )
    .await;
    assert!(json_of(&raw).get("included").is_some());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn errors_are_tool_results() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/apps/404"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errors": [{"status": "404", "code": "NOT_FOUND", "title": "The specified resource does not exist",
                "detail": "There is no resource of type 'apps' with id '404'"}]
        })))
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options())).await;

    let result = call(&client, "get_app", json!({"id": "404"})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("HTTP 404") && message.contains("NOT_FOUND"),
        "{message}"
    );

    // Аргументы проверяются до запроса.
    let result = call(&client, "get_app", json!({"id": "1", "fields[apps]": "x"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("dotted names"), "{}", text(&result));

    // Инструмент выше уровня доступа объясняет, какой флаг нужен.
    let registry = appstore_connect_mcp_core::ToolRegistry::new(&options()).unwrap();
    assert!(registry.iter().all(|t| t.access == AccessLevel::Read));
    let write = appstore_connect_api::spec::operations()
        .iter()
        .find(|op| op.access == AccessLevel::Write && op.toolset == "apps")
        .unwrap();
    let result = call(&client, write.tool_name, json!({})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`write` access"),
        "{}",
        text(&result)
    );

    // Неизвестный инструмент — ошибка протокола.
    let err = client
        .call_tool(rmcp::model::CallToolRequestParams::new("no_such_tool"))
        .await;
    assert!(err.is_err());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn write_tools_send_json_body() {
    let api = MockServer::start().await;
    let op = appstore_connect_api::spec::find("create_beta_group").unwrap();
    let body = json!({"data": {"type": "betaGroups", "attributes": {"name": "QA"},
        "relationships": {"app": {"data": {"type": "apps", "id": "1"}}}}});
    Mock::given(method("POST"))
        .and(path(op.path))
        .and(body_json(&body))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"data": {"type": "betaGroups", "id": "g1"}})),
        )
        .expect(1)
        .mount(&api)
        .await;
    let client = connect(server(
        &api.uri(),
        ServerOptions {
            max_access: AccessLevel::Write,
            ..options()
        },
    ))
    .await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "create_beta_group")
        .unwrap();
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(false)
    );
    let result = call(&client, "create_beta_group", json!({"body": body})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(json_of(&result)["data"]["id"], "g1");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn read_only_server_lists_only_read_tools() {
    let client = connect(server("http://127.0.0.1:9", options())).await;
    let tools = client.list_all_tools().await.unwrap();
    assert!(!tools.is_empty());
    let registry = appstore_connect_mcp_core::ToolRegistry::new(&options()).unwrap();
    assert_eq!(tools.len(), registry.len());
    for tool in &tools {
        let annotations = tool.annotations.as_ref().unwrap();
        assert_eq!(annotations.read_only_hint, Some(true), "{}", tool.name);
    }
    let echo = registry.get("test_echo").unwrap();
    assert!(matches!(echo.kind, ToolKind::Composite(0)));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn composite_tools_get_validated_arguments_and_context() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/apps/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"type": "apps", "id": "1", "attributes": {"name": "Demo"}}
        })))
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options())).await;
    let result = call(&client, "test_echo", json!({"app_id": "1"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(
        json_of(&result),
        json!({"name": "Demo", "profile": "work", "vendor": "80000000", "access": "read"})
    );

    for (args, expected) in [
        (json!({}), "missing required argument(s): `app_id`"),
        (json!({"app_id": 1}), "must be a string"),
        (
            json!({"app_id": "1", "extra": true}),
            "unknown argument(s) `extra`",
        ),
        (
            json!({"app_id": "1", "fail": true}),
            "echo failed on request",
        ),
    ] {
        let result = call(&client, "test_echo", args.clone()).await;
        assert_eq!(result.is_error, Some(true), "{args}");
        assert!(
            text(&result).contains(expected),
            "{args}: {}",
            text(&result)
        );
    }
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn prompts_are_listed_and_rendered() {
    let client = connect(server("http://127.0.0.1:9", options())).await;
    let info = client.peer_info().unwrap();
    assert!(info.capabilities.prompts.is_some());
    let prompts = client.list_all_prompts().await.unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].name, "test_prompt");
    assert_eq!(prompts[0].title.as_deref(), Some("Test prompt"));
    let arguments = prompts[0].arguments.as_ref().unwrap();
    assert_eq!(arguments[0].required, Some(true));

    let result = client
        .get_prompt(
            GetPromptRequestParams::new("test_prompt")
                .with_arguments(common::args(json!({"app": "Demo"}))),
        )
        .await
        .unwrap();
    let message = serde_json::to_value(&result.messages[0]).unwrap();
    assert_eq!(message["role"], "user");
    assert_eq!(
        message["content"]["text"],
        "Check the app Demo (any platform)."
    );

    for args in [json!({}), json!({"app": "Demo", "other": "x"})] {
        let err = client
            .get_prompt(
                GetPromptRequestParams::new("test_prompt").with_arguments(common::args(args)),
            )
            .await;
        assert!(err.is_err());
    }
    assert!(
        client
            .get_prompt(GetPromptRequestParams::new("no_such_prompt"))
            .await
            .is_err()
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn responses_over_the_limit_are_rejected() {
    let api = MockServer::start().await;
    let big = "x".repeat(300 * 1024);
    Mock::given(method("GET"))
        .and(path("/v1/apps/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"type": "apps", "id": "1", "attributes": {"name": big}}
        })))
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options())).await;
    let result = call(&client, "get_app", json!({"id": "1"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("larger than"), "{}", text(&result));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn api_request_follows_next_links_and_saves_json() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/apps"))
        .and(query_param("cursor", "AB"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"type": "apps", "id": "2"}],
            "links": {"self": "x"},
            "meta": {"paging": {"total": 2, "limit": 1}}
        })))
        .mount(&api)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let client = connect(common::server_with(
        &api.uri(),
        options(),
        appstore_connect_mcp_core::FilePolicy::new(std::slice::from_ref(&root)).unwrap(),
        std::sync::Arc::new(appstore_connect_mcp_core::composite::testing::MemoryVault::default()),
    ))
    .await;

    let next = format!("{}/v1/apps?cursor=AB&limit=1", api.uri());
    let result = call(&client, "api_request", json!({ "path": next })).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value = json_of(&result);
    assert_eq!(value["result"]["data"][0]["id"], "2");
    assert_eq!(value["pagination"]["total"], 2);
    assert!(value["pagination"].get("next").is_none());

    let target = root.join("apps.json");
    let result = call(
        &client,
        "api_request",
        json!({"path": "/v1/apps?cursor=AB", "save_to": target.display().to_string()}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value = json_of(&result);
    assert_eq!(value["result"]["saved_to"], target.display().to_string());
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
    assert_eq!(saved["data"][0]["id"], "2");

    // Чужой хост и удаление на сервере только для чтения отклоняются.
    let result = call(
        &client,
        "api_request",
        json!({"path": "https://example.com/v1/apps"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let result = call(
        &client,
        "api_request",
        json!({"method": "DELETE", "path": "/v1/betaGroups/1"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`destructive` access"),
        "{}",
        text(&result)
    );

    let result = call(&client, "api_search", json!({"query": "сборки"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert!(text(&result).contains("build"), "{}", text(&result));
    client.cancel().await.unwrap();
}

/// Настоящие составные инструменты и промпты ([`ServerOptions::default`]) доходят до клиента.
#[tokio::test]
async fn default_composites_and_prompts_are_registered() {
    let options = ServerOptions {
        max_access: AccessLevel::Write,
        toolsets: Some(
            appstore_connect_api::spec::toolsets()
                .iter()
                .map(|toolset| toolset.slug.to_owned())
                .collect(),
        ),
        ..ServerOptions::default()
    };
    let client = connect(server("http://127.0.0.1:9", options)).await;
    let tools: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    for name in [
        "asset_upload",
        "build_upload",
        "certificate_create",
        "metadata_apply",
        "metadata_export",
        "profile_download",
        "release_status",
        "report_download",
    ] {
        assert!(tools.iter().any(|tool| tool == name), "нет {name}");
    }
    let prompts: Vec<String> = client
        .list_all_prompts()
        .await
        .unwrap()
        .into_iter()
        .map(|prompt| prompt.name)
        .collect();
    assert_eq!(
        prompts,
        [
            "prepare_release",
            "triage_reviews",
            "testflight_distribution"
        ]
    );
    client.cancel().await.unwrap();
}
