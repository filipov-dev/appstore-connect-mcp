//! Общее для интеграционных тестов ядра: сервер с ключом, сгенерированным в тесте, против
//! `wiremock`, и MCP-клиент `rmcp` поверх `tokio::io::duplex`. Сеть и системный keychain
//! не используются.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use appstore_connect_api::executor::{ApiClient, ClientConfig};
use appstore_connect_api::external::ExternalClient;
use appstore_connect_api::jwt::JwtTokens;
use appstore_connect_api::{ApiKey, Issuer, IssuerId, KeyId, PrivateKeyPem};
use appstore_connect_mcp_core::composite::testing::MemoryVault;
use appstore_connect_mcp_core::{AppStoreServer, FilePolicy, ServerOptions, Services};
use p256::ecdsa::SigningKey;
use p256::elliptic_curve::Generate;
use p256::pkcs8::{EncodePrivateKey, LineEnding};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;

pub fn tokens() -> Arc<JwtTokens> {
    let pem = SigningKey::generate().to_pkcs8_pem(LineEnding::LF).unwrap();
    let pem = PrivateKeyPem::parse(&pem).unwrap();
    let key = ApiKey::new(
        &pem,
        KeyId::new("TESTKEY123").unwrap(),
        Issuer::Team(IssuerId::new("01234567-89ab-cdef-0123-456789abcdef").unwrap()),
    )
    .unwrap();
    Arc::new(JwtTokens::new(key))
}

pub fn client_config(base_url: &str, max_response_bytes: usize) -> ClientConfig {
    let mut config = ClientConfig::new(Some(tokens())).unwrap();
    config.base_url = base_url.parse().unwrap();
    config.max_response_bytes = max_response_bytes;
    config.retry.max_retries = 0;
    config
}

pub fn server_with(
    base_url: &str,
    options: ServerOptions,
    files: FilePolicy,
    vault: Arc<MemoryVault>,
) -> AppStoreServer {
    let api = ApiClient::new(client_config(base_url, 256 * 1024)).unwrap();
    let external = ExternalClient::new(Duration::from_secs(5), 1024 * 1024)
        .unwrap()
        .allow_loopback_http();
    AppStoreServer::new(
        Services {
            api,
            external,
            vault,
        },
        options,
        files,
    )
    .unwrap()
}

pub fn server(base_url: &str, options: ServerOptions) -> AppStoreServer {
    server_with(
        base_url,
        options,
        FilePolicy::new(&[]).unwrap(),
        Arc::new(MemoryVault::default()),
    )
}

/// MCP-сессия с сервером в памяти.
pub async fn connect(server: AppStoreServer) -> RunningService<RoleClient, ()> {
    let (server_io, client_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    ().serve(client_io).await.unwrap()
}

pub fn args(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("не объект: {other}"),
    }
}

pub async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(arguments)))
        .await
        .unwrap()
}

pub fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block: &ContentBlock| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn json_of(result: &CallToolResult) -> Value {
    let text = text(result);
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("не JSON ({err}): {text}"))
}
