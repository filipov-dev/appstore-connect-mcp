//! Динамические наборы инструментов: начальный список, `enable_toolset` с уведомлением
//! `notifications/tools/list_changed` (через сессию с `initialize` и через подписку протокола
//! 2026-07-28), уровень доступа и пользовательские инструменты. App Store Connect API подменён
//! `wiremock`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use appstore_connect_api::AccessLevel;
use appstore_connect_mcp_core::{AppStoreServer, CustomTool, ServerOptions};
use common::{args, server, text};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, JsonObject, ProtocolVersion, ServerNotification,
    SubscriptionFilter,
};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::{ClientHandler, ClientServiceExt, RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::sync::Notify;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CORE_TOOLS: [&str; 5] = [
    "api_request",
    "api_search",
    "disable_toolset",
    "enable_toolset",
    "list_toolsets",
];

/// Клиент, который считает уведомления о смене списка инструментов.
#[derive(Clone, Default)]
struct CountingClient {
    changes: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl ClientHandler for CountingClient {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changes.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

fn options(max_access: AccessLevel, dynamic: bool) -> ServerOptions {
    ServerOptions {
        max_access,
        dynamic_toolsets: dynamic,
        composite_tools: Vec::new(),
        ..ServerOptions::default()
    }
}

fn spawn(server: AppStoreServer) -> tokio::io::DuplexStream {
    let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    client_io
}

async fn call<S: rmcp::Service<RoleClient>>(
    client: &RunningService<RoleClient, S>,
    name: &str,
    value: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(value)))
        .await
        .unwrap()
}

async fn tool_names<S: rmcp::Service<RoleClient>>(
    client: &RunningService<RoleClient, S>,
) -> Vec<String> {
    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    names
}

/// Главный сценарий: агент включает набор, получает уведомление и сразу вызывает
/// инструмент набора (с фильтром `jq`).
#[tokio::test]
async fn enable_toolset_adds_tools_and_notifies() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/betaGroups"))
        .and(query_param("limit", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"type": "betaGroups", "id": "g1", "attributes": {"name": "QA", "isInternalGroup": true}},
                {"type": "betaGroups", "id": "g2", "attributes": {"name": "Public", "isInternalGroup": false}}
            ],
            "links": {"self": "x"}
        })))
        .expect(1)
        .mount(&api)
        .await;

    let handler = CountingClient::default();
    let client = handler
        .clone()
        .serve(spawn(server(&api.uri(), options(AccessLevel::Read, true))))
        .await
        .unwrap();

    let info = client.peer_info().unwrap();
    let tools_capability = info.capabilities.tools.as_ref().unwrap();
    assert_eq!(tools_capability.list_changed, Some(true));
    let instructions = info.instructions.as_deref().unwrap();
    assert!(instructions.contains("`enable_toolset`"), "{instructions}");
    assert!(
        instructions.contains("enabled at start: none"),
        "{instructions}"
    );

    assert_eq!(tool_names(&client).await, CORE_TOOLS);

    // До включения набора инструмент недоступен, ошибка подсказывает `enable_toolset`.
    let result = call(&client, "list_beta_groups", json!({})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`enable_toolset`") && text(&result).contains("GET /v1/betaGroups"),
        "{}",
        text(&result)
    );

    let listed: Value =
        serde_json::from_str(&text(&call(&client, "list_toolsets", json!({})).await)).unwrap();
    let testflight = listed["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["toolset"] == "testflight")
        .unwrap();
    assert_eq!(testflight["enabled"], false);

    let result = call(&client, "enable_toolset", json!({"toolset": "testflight"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    tokio::time::timeout(Duration::from_secs(5), handler.notify.notified())
        .await
        .expect("tools/list_changed");
    assert_eq!(handler.changes.load(Ordering::SeqCst), 1);

    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "list_beta_groups"), "{names:?}");
    // Уровень доступа действует и для включённого набора.
    assert!(!names.iter().any(|n| n == "create_beta_group"), "{names:?}");

    let result = call(
        &client,
        "list_beta_groups",
        json!({"limit": 2, "jq": "[.data[] | select(.attributes.isInternalGroup) | .attributes.name]"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"["QA"]"#);

    // Повторное включение не меняет список и не шлёт уведомление.
    call(&client, "enable_toolset", json!({"toolset": "testflight"})).await;
    let result = call(&client, "disable_toolset", json!({"toolset": "testflight"})).await;
    assert_eq!(result.is_error, Some(false));
    tokio::time::timeout(Duration::from_secs(5), handler.notify.notified())
        .await
        .expect("tools/list_changed");
    assert_eq!(handler.changes.load(Ordering::SeqCst), 2);
    assert_eq!(tool_names(&client).await, CORE_TOOLS);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn write_tools_appear_only_with_write_access() {
    let api = MockServer::start().await;
    let client =
        ().serve(spawn(server(&api.uri(), options(AccessLevel::Write, true))))
            .await
            .unwrap();
    call(
        &client,
        "enable_toolset",
        json!({"toolset": "testflight,apps"}),
    )
    .await;
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "create_beta_group"), "{names:?}");
    assert!(names.iter().any(|n| n == "get_app"), "{names:?}");
    // Удаление требует `Destructive`.
    assert!(!names.iter().any(|n| n == "delete_beta_group"), "{names:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn toolsets_from_flags_are_enabled_at_start() {
    let api = MockServer::start().await;
    let server = server(
        &api.uri(),
        ServerOptions {
            toolsets: Some(vec!["builds".into()]),
            ..options(AccessLevel::Read, true)
        },
    );
    assert!(server.registry().get("list_builds").is_some());
    assert!(server.registry().get("get_app").is_none());
    assert!(
        server.instructions().contains("enabled at start: builds"),
        "{}",
        server.instructions()
    );
}

#[tokio::test]
async fn subscription_receives_list_changed() {
    let api = MockServer::start().await;
    let client = ()
        .serve_with_lifecycle(
            spawn(server(&api.uri(), options(AccessLevel::Read, true))),
            rmcp::ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();
    let mut subscription = client
        .listen(SubscriptionFilter::builder().tools_list_changed().build())
        .await
        .unwrap();
    assert_eq!(
        subscription.acknowledged(),
        &SubscriptionFilter::builder().tools_list_changed().build()
    );

    let result = call(&client, "enable_toolset", json!({"toolset": "users"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let notification = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .unwrap()
        .unwrap()
        .expect("tools/list_changed");
    assert!(matches!(
        notification,
        ServerNotification::ToolListChangedNotification(_)
    ));
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "list_users"), "{names:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn static_mode_is_unchanged() {
    let api = MockServer::start().await;
    let client =
        ().serve(spawn(server(&api.uri(), options(AccessLevel::Read, false))))
            .await
            .unwrap();
    let info = client.peer_info().unwrap();
    assert_ne!(
        info.capabilities.tools.as_ref().unwrap().list_changed,
        Some(true)
    );
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "get_app"));
    assert!(!names.iter().any(|n| n == "enable_toolset"));
    let result = client
        .call_tool(CallToolRequestParams::new("enable_toolset").with_arguments(JsonObject::new()))
        .await;
    assert!(result.is_err(), "неизвестный инструмент — ошибка протокола");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn clones_share_the_session() {
    let api = MockServer::start().await;
    let first = server(&api.uri(), options(AccessLevel::Read, true));
    first
        .call("enable_toolset", args(json!({"toolset": "apps"})))
        .await
        .unwrap();
    assert!(first.registry().get("get_app").is_some());
    assert!(first.clone().registry().get("get_app").is_some());
}

/// Пользовательские инструменты настроил пользователь: в динамическом режиме они есть сразу,
/// не входят ни в один набор и переживают `enable_toolset`/`disable_toolset`. Уровень
/// доступа действует как обычно.
#[tokio::test]
async fn custom_tools_are_registered_from_start() {
    let custom = |name: &str, operation: &str, fixed: Value| {
        CustomTool::compile(
            serde_json::from_value(json!({
                "name": name,
                "description": "Custom tool",
                "operation": operation,
                "args": fixed
            }))
            .unwrap(),
            "tools.toml",
        )
        .unwrap()
    };
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/apps/123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"type": "apps", "id": "123", "attributes": {"name": "Demo"}}
        })))
        .expect(1)
        .mount(&api)
        .await;
    let server = server(
        &api.uri(),
        ServerOptions {
            custom_tools: vec![
                custom("main_app", "get_app", json!({"id": "123"})),
                custom("qa_group", "create_beta_group", json!({})),
            ],
            ..options(AccessLevel::Read, true)
        },
    );
    let names = |server: &AppStoreServer| {
        let mut names: Vec<String> = server
            .registry()
            .iter()
            .map(|t| t.tool.name.to_string())
            .collect();
        names.sort();
        names
    };
    let mut expected: Vec<String> = CORE_TOOLS.iter().map(|s| s.to_string()).collect();
    expected.push("main_app".to_owned());
    expected.sort();
    assert_eq!(names(&server), expected);
    // Выше уровня доступа — не зарегистрирован, вызов объясняет, какой уровень нужен.
    let result = server
        .call("qa_group", args(json!({"body": {}})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`write` access"),
        "{}",
        text(&result)
    );

    for tool in ["enable_toolset", "disable_toolset"] {
        let result = server
            .call(tool, args(json!({"toolset": "apps"})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{}", text(&result));
        assert!(server.registry().get("main_app").is_some(), "{tool}");
        // Пользовательские инструменты не попадают в списки добавленных и убранных.
        assert!(!text(&result).contains("main_app"), "{}", text(&result));
    }

    let result = server
        .call("main_app", args(json!({"jq": ".data.attributes.name"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#""Demo""#);
    // Фиксированный аргумент агент не подменяет.
    let result = server
        .call("main_app", args(json!({"id": "999"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("fixed"), "{}", text(&result));
}
