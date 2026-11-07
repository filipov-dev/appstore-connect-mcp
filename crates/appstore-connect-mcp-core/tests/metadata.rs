//! Тесты `metadata_export` и `metadata_apply` на `wiremock` и временном каталоге.
//!
//! Данные синтетические: ключ ES256 генерируется в тесте, ID приложения, версий и
//! локализаций вымышленные.

use std::{path::Path, sync::Arc, time::Duration};

use appstore_connect_mcp_core::appstore_connect_api::{
    AccessLevel, ApiKey, Issuer, IssuerId, JwtTokens, KeyId, PrivateKeyPem,
    executor::{ApiClient, ClientConfig, RetryPolicy},
    external::ExternalClient,
};
use appstore_connect_mcp_core::composite::{
    self, CompositeTool, Context, ProfileInfo,
    testing::{DirFiles, MemoryVault},
};
use p256::{
    ecdsa::SigningKey,
    elliptic_curve::Generate,
    pkcs8::{EncodePrivateKey, LineEnding},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, query_param, query_param_is_missing},
};

const APP: &str = "1000000001";
const VERSION: &str = "ver-editable";
const INFO: &str = "info-editable";

fn context(server: &MockServer, root: &Path, access: AccessLevel) -> Context {
    let signing = SigningKey::generate();
    let pem = signing.to_pkcs8_pem(LineEnding::LF).unwrap();
    let key = ApiKey::new(
        &PrivateKeyPem::parse(&pem).unwrap(),
        KeyId::new("TESTKEY123").unwrap(),
        Issuer::Team(IssuerId::new("01234567-89ab-cdef-0123-456789abcdef").unwrap()),
    )
    .unwrap();
    let config = ClientConfig {
        base_url: url::Url::parse(&server.uri()).unwrap(),
        tokens: Some(Arc::new(JwtTokens::new(key))),
        timeout: Duration::from_secs(5),
        max_response_bytes: 1024 * 1024,
        retry: RetryPolicy {
            max_retries: 0,
            default_delay: Duration::from_millis(10),
            max_delay: Duration::from_secs(1),
        },
    };
    Context {
        api: Arc::new(ApiClient::new(config).unwrap()),
        external: Arc::new(
            ExternalClient::new(Duration::from_secs(5), 1024 * 1024)
                .unwrap()
                .allow_loopback_http(),
        ),
        files: Arc::new(DirFiles::new(root)),
        vault: Arc::new(MemoryVault::default()),
        profile: ProfileInfo::default(),
        access,
    }
}

fn tool(name: &str) -> CompositeTool {
    composite::all()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap()
}

async fn call(ctx: &Context, name: &str, args: Value) -> Result<Value, String> {
    (tool(name).run)(ctx, args).await.map_err(|e| e.message)
}

/// Состояние App Store Connect для моков.
#[derive(Clone)]
struct State {
    version_string: String,
    /// (id, locale, attributes)
    version_locs: Vec<(String, String, Value)>,
    info_locs: Vec<(String, String, Value)>,
}

fn initial_state() -> State {
    State {
        version_string: "1.2.0".into(),
        version_locs: vec![
            (
                "vl-en".into(),
                "en-US".into(),
                json!({
                    "description": "An app for testing.\nSecond line.",
                    "keywords": "test,demo",
                    "marketingUrl": null,
                    "promotionalText": "Try it",
                    "supportUrl": "https://example.com/support",
                    "whatsNew": "Bug fixes"
                }),
            ),
            (
                "vl-de".into(),
                "de-DE".into(),
                json!({
                    "description": "Eine Test-App.",
                    "keywords": "test",
                    "supportUrl": "https://example.com/de/support",
                    "whatsNew": ""
                }),
            ),
        ],
        info_locs: vec![
            (
                "il-en".into(),
                "en-US".into(),
                json!({
                    "name": "Demo App",
                    "subtitle": "Testing things",
                    "privacyPolicyUrl": "https://example.com/privacy"
                }),
            ),
            (
                "il-de".into(),
                "de-DE".into(),
                json!({ "name": "Demo App DE", "subtitle": null }),
            ),
        ],
    }
}

fn loc_item(kind: &str, id: &str, locale: &str, attrs: &Value) -> Value {
    let mut attributes = attrs.clone();
    attributes["locale"] = json!(locale);
    json!({ "type": kind, "id": id, "attributes": attributes })
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

/// Моки чтения. Локализации версии отдаются двумя страницами (`links.next`).
async fn mount(server: &MockServer, state: &State) {
    let version = |id: &str, vs: &str, st: &str| {
        json!({
            "type": "appStoreVersions",
            "id": id,
            "attributes": { "platform": "IOS", "versionString": vs, "appVersionState": st }
        })
    };
    Mock::given(method("GET"))
        .and(path(format!("/v1/apps/{APP}/appStoreVersions")))
        .and(query_param("filter[platform]", "IOS"))
        .respond_with(ok(json!({
            "data": [
                version("ver-live", "1.1.0", "READY_FOR_DISTRIBUTION"),
                version(VERSION, &state.version_string, "PREPARE_FOR_SUBMISSION"),
            ],
            "links": { "self": "x" }
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/appStoreVersions/{VERSION}")))
        .respond_with(ok(json!({
            "data": version(VERSION, &state.version_string, "PREPARE_FOR_SUBMISSION")
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/appStoreVersions/ver-live"))
        .respond_with(ok(json!({
            "data": version("ver-live", "1.1.0", "READY_FOR_DISTRIBUTION")
        })))
        .mount(server)
        .await;
    let info =
        |id: &str, st: &str| json!({ "type": "appInfos", "id": id, "attributes": { "state": st } });
    Mock::given(method("GET"))
        .and(path(format!("/v1/apps/{APP}/appInfos")))
        .respond_with(ok(json!({
            "data": [info("info-live", "READY_FOR_DISTRIBUTION"), info(INFO, "PREPARE_FOR_SUBMISSION")]
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/appInfos/{INFO}")))
        .respond_with(ok(json!({ "data": info(INFO, "PREPARE_FOR_SUBMISSION") })))
        .mount(server)
        .await;

    // Локализации версии: по одной на страницу.
    let list = format!("/v1/appStoreVersions/{VERSION}/appStoreVersionLocalizations");
    let items: Vec<Value> = state
        .version_locs
        .iter()
        .map(|(id, locale, attrs)| loc_item("appStoreVersionLocalizations", id, locale, attrs))
        .collect();
    let (first, rest) = items
        .split_first()
        .map_or((vec![], vec![]), |(f, r)| (vec![f.clone()], r.to_vec()));
    let next = format!("{}{list}?cursor=page2&limit=200", server.uri());
    let first_links = if rest.is_empty() {
        json!({ "self": "x" })
    } else {
        json!({ "self": "x", "next": next })
    };
    Mock::given(method("GET"))
        .and(path(list.clone()))
        .and(query_param_is_missing("cursor"))
        .respond_with(ok(json!({ "data": first, "links": first_links })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(list))
        .and(query_param("cursor", "page2"))
        .respond_with(ok(json!({ "data": rest, "links": { "self": "x" } })))
        .mount(server)
        .await;

    let items: Vec<Value> = state
        .info_locs
        .iter()
        .map(|(id, locale, attrs)| loc_item("appInfoLocalizations", id, locale, attrs))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/v1/appInfos/{INFO}/appInfoLocalizations")))
        .respond_with(ok(json!({ "data": items, "links": { "self": "x" } })))
        .mount(server)
        .await;
}

/// Моки записи: любые POST/PATCH/DELETE локализаций успешны.
async fn mount_writes(server: &MockServer) {
    for kind in ["appStoreVersionLocalizations", "appInfoLocalizations"] {
        Mock::given(method("POST"))
            .and(path(format!("/v1/{kind}")))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({ "data": { "type": kind, "id": "new-id" } })),
            )
            .mount(server)
            .await;
        Mock::given(method("PATCH"))
            .respond_with(ok(json!({ "data": { "type": kind, "id": "x" } })))
            .mount(server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(204))
            .mount(server)
            .await;
    }
}

async fn writes(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() != "GET")
        .collect()
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn write(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

/// Экспорт в `<tmp>/meta`; возвращает путь каталога.
async fn export(ctx: &Context, tmp: &TempDir) -> std::path::PathBuf {
    call(
        ctx,
        "metadata_export",
        json!({ "app_id": APP, "dir": "meta" }),
    )
    .await
    .unwrap();
    tmp.path().join("meta")
}

#[test]
fn tools_are_registered_with_access_levels() {
    let export = tool("metadata_export");
    assert_eq!(export.access, AccessLevel::Read);
    assert_eq!(export.toolset, "metadata");
    assert!(export.uses_local_files);
    let apply = tool("metadata_apply");
    assert_eq!(apply.access, AccessLevel::Write);
    assert!(apply.uses_local_files);
    let schema = (apply.input_schema)();
    assert_eq!(schema["properties"]["dry_run"]["default"], true);
    assert_eq!(schema["additionalProperties"], false);
}

#[tokio::test]
async fn export_writes_one_file_per_locale_and_follows_pages() {
    let server = MockServer::start().await;
    mount(&server, &initial_state()).await;
    let tmp = TempDir::new().unwrap();
    let ctx = context(&server, tmp.path(), AccessLevel::Read);

    let out = call(
        &ctx,
        "metadata_export",
        json!({ "app_id": APP, "dir": "meta" }),
    )
    .await
    .unwrap();
    assert_eq!(out["locales"], json!(["de-DE", "en-US"]));
    assert_eq!(
        out["files"],
        json!(["de-DE.json", "en-US.json", "_meta.json"])
    );
    assert_eq!(out["version"]["version_string"], "1.2.0");
    assert_eq!(out["version"]["id"], VERSION);
    assert_eq!(out["app_info"]["id"], INFO);
    // Тексты в ответ не попадают.
    assert!(!out.to_string().contains("An app for testing"));

    let dir = tmp.path().join("meta");
    let en = read(&dir.join("en-US.json"));
    assert_eq!(
        en["version"]["description"],
        "An app for testing.\nSecond line."
    );
    assert_eq!(en["version"]["marketingUrl"], Value::Null);
    assert_eq!(en["appInfo"]["name"], "Demo App");
    assert_eq!(en["appInfo"]["privacyChoicesUrl"], Value::Null);
    // de-DE пришла со второй страницы.
    let de = read(&dir.join("de-DE.json"));
    assert_eq!(de["version"]["description"], "Eine Test-App.");
    assert_eq!(de["version"]["whatsNew"], Value::Null);
    assert_eq!(de["appInfo"]["name"], "Demo App DE");

    let meta = read(&dir.join("_meta.json"));
    assert_eq!(meta["app_id"], APP);
    assert_eq!(meta["version_id"], VERSION);
    assert_eq!(meta["version_string"], "1.2.0");
    assert_eq!(meta["app_info_id"], INFO);
    assert!(meta["exported_at"].as_str().unwrap().ends_with('Z'));

    // Ключи в порядке спецификации, а не по алфавиту.
    let text = std::fs::read_to_string(dir.join("en-US.json")).unwrap();
    assert!(text.find("\"version\"").unwrap() < text.find("\"appInfo\"").unwrap());
    assert!(text.find("\"name\"").unwrap() < text.find("\"subtitle\"").unwrap());

    // Повторный экспорт в тот же каталог отклоняется.
    let err = call(
        &ctx,
        "metadata_export",
        json!({ "app_id": APP, "dir": "meta" }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("already contains"), "{err}");
}

#[tokio::test]
async fn export_reports_ambiguous_versions() {
    let server = MockServer::start().await;
    let v = |id: &str| {
        json!({ "type": "appStoreVersions", "id": id,
                "attributes": { "versionString": id, "appVersionState": "PREPARE_FOR_SUBMISSION" } })
    };
    Mock::given(method("GET"))
        .and(path(format!("/v1/apps/{APP}/appStoreVersions")))
        .respond_with(ok(json!({ "data": [v("v-a"), v("v-b")] })))
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let ctx = context(&server, tmp.path(), AccessLevel::Read);
    let err = call(
        &ctx,
        "metadata_export",
        json!({ "app_id": APP, "dir": "meta" }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("several editable"), "{err}");
    assert!(err.contains("v-a") && err.contains("v-b"), "{err}");
    assert!(err.contains("version_id"), "{err}");
}

#[tokio::test]
async fn edit_dry_run_apply_and_reapply() {
    let server = MockServer::start().await;
    let mut state = initial_state();
    mount(&server, &state).await;
    mount_writes(&server).await;
    let tmp = TempDir::new().unwrap();
    let ctx = context(&server, tmp.path(), AccessLevel::Write);
    let dir = export(&ctx, &tmp).await;

    // Без правок — пустой план.
    let out = call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "dry_run": false }),
    )
    .await
    .unwrap();
    assert_eq!(out["summary"]["write_requests"], 0);
    assert!(writes(&server).await.is_empty());

    // Правим одно поле.
    let mut en = read(&dir.join("en-US.json"));
    en["version"]["description"] = json!("A new description.");
    write(&dir.join("en-US.json"), &en);

    let out = call(&ctx, "metadata_apply", json!({ "dir": "meta" }))
        .await
        .unwrap();
    assert_eq!(out["dry_run"], true);
    assert_eq!(
        out["changes"],
        json!([{
            "locale": "en-US",
            "resource": "version",
            "action": "update",
            "fields": [{
                "field": "description",
                "old": "An app for testing.⏎Second line.",
                "new": "A new description."
            }]
        }])
    );
    assert_eq!(out["summary"]["update"], 1);
    assert_eq!(out["summary"]["fields_changed"], 1);
    assert!(writes(&server).await.is_empty(), "dry run must not write");

    let out = call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "dry_run": false }),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"], 1);
    assert_eq!(out["failed"], 0);
    let sent = writes(&server).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method.as_str(), "PATCH");
    assert_eq!(sent[0].url.path(), "/v1/appStoreVersionLocalizations/vl-en");
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(
        body,
        json!({ "data": {
            "type": "appStoreVersionLocalizations",
            "id": "vl-en",
            "attributes": { "description": "A new description." }
        }})
    );

    // В App Store Connect теперь новое значение: повторное применение ничего не пишет.
    state.version_locs[0].2["description"] = json!("A new description.");
    server.reset().await;
    mount(&server, &state).await;
    mount_writes(&server).await;
    let out = call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "dry_run": false }),
    )
    .await
    .unwrap();
    assert_eq!(out["summary"]["write_requests"], 0);
    assert_eq!(out["unchanged_locales"], json!(["de-DE", "en-US"]));
    assert!(writes(&server).await.is_empty());
}

#[tokio::test]
async fn clearing_a_field_sends_null() {
    let server = MockServer::start().await;
    mount(&server, &initial_state()).await;
    mount_writes(&server).await;
    let tmp = TempDir::new().unwrap();
    let ctx = context(&server, tmp.path(), AccessLevel::Write);
    let dir = export(&ctx, &tmp).await;

    let mut en = read(&dir.join("en-US.json"));
    en["appInfo"]["subtitle"] = Value::Null;
    // Удалённое из файла поле не трогаем.
    en["version"].as_object_mut().unwrap().remove("keywords");
    write(&dir.join("en-US.json"), &en);

    call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "dry_run": false }),
    )
    .await
    .unwrap();
    let sent = writes(&server).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].url.path(), "/v1/appInfoLocalizations/il-en");
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["data"]["attributes"], json!({ "subtitle": null }));
}

#[tokio::test]
async fn new_locale_is_created() {
    let server = MockServer::start().await;
    mount(&server, &initial_state()).await;
    mount_writes(&server).await;
    let tmp = TempDir::new().unwrap();
    let ctx = context(&server, tmp.path(), AccessLevel::Write);
    let dir = export(&ctx, &tmp).await;

    write(
        &dir.join("fr-FR.json"),
        &json!({
            "version": { "description": "Une application de test.", "keywords": null },
            "appInfo": { "name": "Démo", "subtitle": "Tester" }
        }),
    );

    let out = call(&ctx, "metadata_apply", json!({ "dir": "meta" }))
        .await
        .unwrap();
    assert_eq!(out["summary"]["create"], 2);
    assert!(writes(&server).await.is_empty());

    let out = call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "dry_run": false }),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"], 2);
    let sent = writes(&server).await;
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|r| r.method.as_str() == "POST"));

    let version_body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(sent[0].url.path(), "/v1/appStoreVersionLocalizations");
    assert_eq!(
        version_body,
        json!({ "data": {
            "type": "appStoreVersionLocalizations",
            "attributes": { "locale": "fr-FR", "description": "Une application de test." },
            "relationships": { "appStoreVersion": { "data": { "type": "appStoreVersions", "id": VERSION } } }
        }})
    );
    let info_body: Value = serde_json::from_slice(&sent[1].body).unwrap();
    assert_eq!(sent[1].url.path(), "/v1/appInfoLocalizations");
    assert_eq!(
        info_body["data"]["attributes"],
        json!({ "locale": "fr-FR", "name": "Démo", "subtitle": "Tester" })
    );
    assert_eq!(
        info_body["data"]["relationships"]["appInfo"]["data"],
        json!({ "type": "appInfos", "id": INFO })
    );
}

#[tokio::test]
async fn invalid_files_send_no_requests() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let ctx = context(&server, tmp.path(), AccessLevel::Write);
    let dir = tmp.path().join("meta");
    std::fs::create_dir(&dir).unwrap();
    write(
        &dir.join("_meta.json"),
        &json!({ "app_id": APP, "version_id": VERSION, "version_string": "1.2.0", "app_info_id": INFO }),
    );
    write(
        &dir.join("en-US.json"),
        &json!({
            "version": {
                "keywords": "k".repeat(101),
                "supportUrl": "not a url",
                "promotionalText": "п".repeat(170)
            },
            "appInfo": { "name": "N".repeat(31), "subtitle": "ok" }
        }),
    );

    let err = call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "dry_run": false }),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("`version.keywords` is 101 characters long, the limit is 100"),
        "{err}"
    );
    assert!(
        err.contains("`appInfo.name` is 31 characters long, the limit is 30"),
        "{err}"
    );
    assert!(
        err.contains("`version.supportUrl` is not an http(s) URL"),
        "{err}"
    );
    // 170 символов кириллицы укладываются в лимит.
    assert!(!err.contains("promotionalText"), "{err}");
    assert!(server.received_requests().await.unwrap().is_empty());

    // Неизвестное поле — тоже ошибка без запросов.
    write(
        &dir.join("en-US.json"),
        &json!({ "version": { "descripton": "typo" } }),
    );
    let err = call(&ctx, "metadata_apply", json!({ "dir": "meta" }))
        .await
        .unwrap_err();
    assert!(err.contains("unknown field `version.descripton`"), "{err}");
    write(&dir.join("en-US.json"), &json!({ "screenshots": {} }));
    let err = call(&ctx, "metadata_apply", json!({ "dir": "meta" }))
        .await
        .unwrap_err();
    assert!(err.contains("unknown section `screenshots`"), "{err}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn missing_locales_are_deleted_only_with_destructive_access() {
    let server = MockServer::start().await;
    mount(&server, &initial_state()).await;
    mount_writes(&server).await;
    let tmp = TempDir::new().unwrap();
    let ctx = context(&server, tmp.path(), AccessLevel::Write);
    let dir = export(&ctx, &tmp).await;
    std::fs::remove_file(dir.join("de-DE.json")).unwrap();

    // Без флага локаль только помечается.
    let out = call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "dry_run": false }),
    )
    .await
    .unwrap();
    assert_eq!(out["missing_locales"], json!(["de-DE"]));
    assert_eq!(out["summary"]["delete"], 0);
    assert!(writes(&server).await.is_empty());

    // С флагом, но без уровня destructive — ошибка до любых запросов.
    let before = server.received_requests().await.unwrap().len();
    let err = call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "dry_run": false, "delete_missing_locales": true }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("--allow-destructive"), "{err}");
    assert_eq!(server.received_requests().await.unwrap().len(), before);

    // Пробный прогон показывает удаления и без уровня destructive.
    let out = call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "delete_missing_locales": true }),
    )
    .await
    .unwrap();
    assert_eq!(out["summary"]["delete"], 2);
    assert!(writes(&server).await.is_empty());

    let ctx = context(&server, tmp.path(), AccessLevel::Destructive);
    let out = call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "dry_run": false, "delete_missing_locales": true }),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"], 2);
    let sent = writes(&server).await;
    let mut deleted: Vec<String> = sent
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    deleted.sort();
    assert_eq!(
        deleted,
        vec![
            "DELETE /v1/appInfoLocalizations/il-de",
            "DELETE /v1/appStoreVersionLocalizations/vl-de",
        ]
    );
}

#[tokio::test]
async fn version_mismatch_needs_explicit_override() {
    let server = MockServer::start().await;
    let mut state = initial_state();
    mount(&server, &state).await;
    let tmp = TempDir::new().unwrap();
    let ctx = context(&server, tmp.path(), AccessLevel::Write);
    export(&ctx, &tmp).await;

    // Версию переименовали после экспорта.
    state.version_string = "1.3.0".into();
    server.reset().await;
    mount(&server, &state).await;
    let err = call(&ctx, "metadata_apply", json!({ "dir": "meta" }))
        .await
        .unwrap_err();
    assert!(err.contains("exported for version 1.2.0"), "{err}");
    assert!(err.contains("version_id"), "{err}");

    let out = call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "version_id": VERSION }),
    )
    .await
    .unwrap();
    assert_eq!(out["version"]["version_string"], "1.3.0");

    // Неизменяемая версия без явного выбора отклоняется.
    let meta_path = tmp.path().join("meta/_meta.json");
    let mut meta = read(&meta_path);
    meta["version_id"] = json!("ver-live");
    meta["version_string"] = json!("1.1.0");
    write(&meta_path, &meta);
    let err = call(&ctx, "metadata_apply", json!({ "dir": "meta" }))
        .await
        .unwrap_err();
    assert!(err.contains("READY_FOR_DISTRIBUTION"), "{err}");
}

#[tokio::test]
async fn partial_failure_is_reported() {
    let server = MockServer::start().await;
    mount(&server, &initial_state()).await;
    Mock::given(method("PATCH"))
        .and(path("/v1/appStoreVersionLocalizations/vl-de"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "errors": [{ "status": "409", "code": "STATE_ERROR", "title": "Not editable", "detail": "The version is not editable." }]
        })))
        .mount(&server)
        .await;
    mount_writes(&server).await;
    let tmp = TempDir::new().unwrap();
    let ctx = context(&server, tmp.path(), AccessLevel::Write);
    let dir = export(&ctx, &tmp).await;
    for locale in ["de-DE", "en-US"] {
        let file = dir.join(format!("{locale}.json"));
        let mut value = read(&file);
        value["version"]["whatsNew"] = json!(format!("New in {locale}"));
        write(&file, &value);
    }

    let err = call(
        &ctx,
        "metadata_apply",
        json!({ "dir": "meta", "dry_run": false }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("1 of 2 changes applied, 1 failed"), "{err}");
    assert!(err.contains("\"status\":\"applied\""), "{err}");
    assert!(err.contains("\"status\":\"failed\""), "{err}");
}
