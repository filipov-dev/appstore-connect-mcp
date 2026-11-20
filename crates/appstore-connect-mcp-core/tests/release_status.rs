//! Тесты `release_status` на `wiremock`: данные синтетические, ключ ES256 генерируется в тесте.

use std::{path::Path, sync::Arc, time::Duration};

use appstore_connect_api::{
    AccessLevel, ApiKey, Issuer, IssuerId, JwtTokens, KeyId, PrivateKeyPem,
    executor::{ApiClient, ClientConfig, RetryPolicy},
    external::ExternalClient,
};
use appstore_connect_mcp_core::composite::{
    self, Context, ProfileInfo,
    release_status::{self, NAME},
    testing::{DirFiles, MemoryVault},
};
use p256::ecdsa::SigningKey;
use p256::elliptic_curve::Generate;
use p256::pkcs8::{EncodePrivateKey, LineEnding};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP: &str = "1000000001";
/// Сколько запросов делает инструмент.
const MAX_REQUESTS: usize = 4;

fn context(server: &MockServer) -> Context {
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
        max_response_bytes: 64 * 1024,
        retry: RetryPolicy {
            max_retries: 0,
            default_delay: Duration::from_millis(10),
            max_delay: Duration::from_secs(1),
        },
    };
    Context {
        api: Arc::new(ApiClient::new(config).unwrap()),
        external: Arc::new(
            ExternalClient::new(Duration::from_secs(5), 1024)
                .unwrap()
                .allow_loopback_http(),
        ),
        files: Arc::new(DirFiles::new(Path::new("/nonexistent"))),
        vault: Arc::new(MemoryVault::default()),
        profile: ProfileInfo::default(),
        access: AccessLevel::Read,
    }
}

async fn call(ctx: &Context, args: Value) -> Result<Value, composite::ToolError> {
    let tool = composite::all()
        .into_iter()
        .find(|t| t.name == NAME)
        .unwrap();
    (tool.run)(ctx, args).await
}

fn app_body() -> Value {
    json!({"data": {"type": "apps", "id": APP, "attributes": {
        "name": "Sample App", "bundleId": "com.example.sample", "sku": "SAMPLE1"}}})
}

fn empty_list() -> Value {
    json!({"data": [], "included": [], "links": {"self": "x"}, "meta": {"paging": {"total": 0, "limit": 20}}})
}

fn version(
    id: &str,
    string: &str,
    state: &str,
    created: &str,
    build: Option<&str>,
    phased: Option<&str>,
) -> Value {
    json!({
        "type": "appStoreVersions", "id": id,
        "attributes": {
            "platform": "IOS", "versionString": string, "appVersionState": state,
            "appStoreState": if state == "READY_FOR_DISTRIBUTION" { "READY_FOR_SALE" } else { state },
            "releaseType": "AFTER_APPROVAL", "earliestReleaseDate": null, "createdDate": created
        },
        "relationships": {
            "build": {"data": build.map(|b| json!({"type": "builds", "id": b}))},
            "appStoreVersionPhasedRelease": {"data": phased.map(|p| json!({"type": "appStoreVersionPhasedReleases", "id": p}))}
        }
    })
}

/// Сборка и её `buildBetaDetail`.
fn build(
    id: &str,
    number: &str,
    state: &str,
    uploaded: &str,
    beta: &str,
    internal: &str,
    external: &str,
) -> (Value, Value) {
    let build = json!({
        "type": "builds", "id": id,
        "attributes": {"version": number, "uploadedDate": uploaded, "expirationDate": "2027-01-01T00:00:00Z",
            "expired": false, "processingState": state, "usesNonExemptEncryption": false,
            "buildAudienceType": "APP_STORE_ELIGIBLE"},
        "relationships": {
            "preReleaseVersion": {"data": {"type": "preReleaseVersions", "id": "prv-24"}},
            "buildBetaDetail": {"data": {"type": "buildBetaDetails", "id": beta}}
        }
    });
    let detail = json!({"type": "buildBetaDetails", "id": beta,
        "attributes": {"internalBuildState": internal, "externalBuildState": external}});
    (build, detail)
}

/// Сборки в формате выдачи JSON:API: `buildBetaDetail` и версия — в `included`.
fn builds_body(builds: Vec<(Value, Value)>) -> Value {
    let mut included = vec![json!({"type": "preReleaseVersions", "id": "prv-24",
        "attributes": {"version": "2.4.0", "platform": "IOS"}})];
    let mut data = Vec::new();
    for (build, detail) in builds {
        data.push(build);
        included.push(detail);
    }
    json!({"data": data, "included": included})
}

async fn mount(server: &MockServer, p: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn mount_full(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/v1/apps/{APP}")))
        .and(query_param("fields[apps]", "name,bundleId,sku"))
        .respond_with(ResponseTemplate::new(200).set_body_json(app_body()))
        .mount(server)
        .await;

    let versions = json!({
        "data": [
            version("v-live", "2.3.0", "READY_FOR_DISTRIBUTION", "2026-09-01T10:00:00Z", Some("b-23"), Some("pr-23")),
            version("v-next", "2.4.0", "WAITING_FOR_REVIEW", "2026-10-20T10:00:00Z", Some("b-2"), None)
        ],
        "included": [
            {"type": "builds", "id": "b-23", "attributes": {"version": "230", "processingState": "VALID",
                "uploadedDate": "2026-08-30T10:00:00Z", "expired": false, "usesNonExemptEncryption": false}},
            {"type": "builds", "id": "b-2", "attributes": {"version": "241", "processingState": "VALID",
                "uploadedDate": "2026-10-21T10:00:00Z", "expired": false, "usesNonExemptEncryption": false}},
            {"type": "appStoreVersionPhasedReleases", "id": "pr-23", "attributes": {
                "phasedReleaseState": "ACTIVE", "startDate": "2026-11-05T00:00:00Z",
                "totalPauseDuration": 0, "currentDayNumber": 3}}
        ]
    });
    Mock::given(method("GET"))
        .and(path(format!("/v1/apps/{APP}/appStoreVersions")))
        .and(query_param("filter[platform]", "IOS"))
        .and(query_param("include", "build,appStoreVersionPhasedRelease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(versions))
        .mount(server)
        .await;

    let submissions = json!({
        "data": [
            {"type": "reviewSubmissions", "id": "rs-old", "attributes": {"platform": "IOS",
                "state": "COMPLETE", "submittedDate": "2026-08-31T10:00:00Z"},
             "relationships": {"items": {"data": [{"type": "reviewSubmissionItems", "id": "i-1"}]},
                "appStoreVersionForReview": {"data": {"type": "appStoreVersions", "id": "v-live"}}}},
            {"type": "reviewSubmissions", "id": "rs-new", "attributes": {"platform": "IOS",
                "state": "WAITING_FOR_REVIEW", "submittedDate": "2026-10-22T10:00:00Z"},
             "relationships": {"items": {"data": [{"type": "reviewSubmissionItems", "id": "i-2"}],
                    "meta": {"paging": {"total": 1, "limit": 50}}},
                "appStoreVersionForReview": {"data": {"type": "appStoreVersions", "id": "v-next"}}}}
        ],
        "included": [
            {"type": "appStoreVersions", "id": "v-next", "attributes": {"versionString": "2.4.0"}}
        ]
    });
    Mock::given(method("GET"))
        .and(path("/v1/reviewSubmissions"))
        .and(query_param("filter[app]", APP))
        .and(query_param("filter[platform]", "IOS"))
        .respond_with(ResponseTemplate::new(200).set_body_json(submissions))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/builds"))
        .and(query_param("filter[app]", APP))
        .and(query_param("filter[preReleaseVersion.platform]", "IOS"))
        .and(query_param("sort", "-uploadedDate"))
        .and(query_param("limit", "5"))
        .and(query_param("include", "preReleaseVersion,buildBetaDetail"))
        .respond_with(ResponseTemplate::new(200).set_body_json(builds_body(vec![
            build(
                "b-3",
                "242",
                "PROCESSING",
                "2026-11-06T10:00:00Z",
                "bd-3",
                "PROCESSING",
                "PROCESSING",
            ),
            build(
                "b-2",
                "241",
                "VALID",
                "2026-10-21T10:00:00Z",
                "bd-2",
                "IN_BETA_TESTING",
                "BETA_APPROVED",
            ),
        ])))
        .mount(server)
        .await;
}

#[tokio::test]
async fn full_release_overview() {
    let server = MockServer::start().await;
    mount_full(&server).await;
    let ctx = context(&server);

    let out = call(&ctx, json!({"app_id": APP})).await.unwrap();

    assert_eq!(
        out["app"],
        json!({"id": APP, "name": "Sample App", "bundle_id": "com.example.sample", "sku": "SAMPLE1"})
    );
    assert_eq!(out["platform"], "IOS");

    let live = &out["live_version"];
    assert_eq!(live["id"], "v-live");
    assert_eq!(live["version_string"], "2.3.0");
    assert_eq!(live["app_store_state"], "READY_FOR_SALE");
    assert_eq!(live["build"]["version"], "230");
    assert_eq!(live["phased_release"]["state"], "ACTIVE");
    assert_eq!(live["phased_release"]["current_day_number"], 3);
    assert_eq!(live["phased_release"]["percent"], 5);

    let pending = &out["pending_version"];
    assert_eq!(pending["id"], "v-next");
    assert_eq!(pending["app_version_state"], "WAITING_FOR_REVIEW");
    assert_eq!(pending["release_type"], "AFTER_APPROVAL");
    assert_eq!(pending["build"]["id"], "b-2");
    assert_eq!(pending["build"]["processing_state"], "VALID");
    assert!(pending["phased_release"].is_null());

    let review = &out["review"];
    assert_eq!(review["id"], "rs-new");
    assert_eq!(review["state"], "WAITING_FOR_REVIEW");
    assert_eq!(review["items_count"], 1);
    assert_eq!(review["app_store_version_id"], "v-next");
    assert_eq!(review["app_store_version_string"], "2.4.0");
    assert_eq!(review["open_submissions"], 1);

    let builds = out["testflight_builds"].as_array().unwrap();
    assert_eq!(builds.len(), 2);
    assert_eq!(builds[0]["id"], "b-3");
    assert_eq!(builds[0]["processing_state"], "PROCESSING");
    assert_eq!(builds[1]["app_version"], "2.4.0");
    assert_eq!(builds[1]["internal_state"], "IN_BETA_TESTING");
    assert_eq!(builds[1]["external_state"], "BETA_APPROVED");

    let steps = out["next_steps"].as_array().unwrap();
    let text: Vec<&str> = steps.iter().filter_map(Value::as_str).collect();
    assert!(
        text.iter().any(|s| s.contains("still processing")),
        "{text:?}"
    );
    assert!(
        text.iter().any(|s| s.contains("with App Review")),
        "{text:?}"
    );
    assert!(
        text.iter()
            .any(|s| s.contains("update_app_store_version_phased_release")),
        "{text:?}"
    );

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests.len() <= MAX_REQUESTS,
        "{} requests",
        requests.len()
    );
    // Заменённые версии не запрашиваются: фильтр по состоянию без REPLACED_WITH_NEW_VERSION.
    let versions = requests
        .iter()
        .find(|r| r.url.path().ends_with("/appStoreVersions"))
        .unwrap();
    let states = versions
        .url
        .query_pairs()
        .find(|(k, _)| k == "filter[appVersionState]")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    assert!(states.contains("READY_FOR_DISTRIBUTION"));
    assert!(!states.contains("REPLACED_WITH_NEW_VERSION"));
}

#[tokio::test]
async fn nothing_in_progress_is_not_an_error() {
    let server = MockServer::start().await;
    mount(&server, &format!("/v1/apps/{APP}"), app_body()).await;
    mount(
        &server,
        &format!("/v1/apps/{APP}/appStoreVersions"),
        empty_list(),
    )
    .await;
    mount(&server, "/v1/reviewSubmissions", empty_list()).await;
    mount(&server, "/v1/builds", json!({"data": []})).await;
    let ctx = context(&server);

    let out = call(
        &ctx,
        json!({"app_id": APP, "platform": "MAC_OS", "builds_limit": 3}),
    )
    .await
    .unwrap();

    assert_eq!(out["platform"], "MAC_OS");
    assert!(out["live_version"].is_null());
    assert!(out["pending_version"].is_null());
    assert!(out["review"].is_null());
    assert_eq!(out["testflight_builds"], json!([]));
    let steps = out["next_steps"].to_string();
    assert!(steps.contains("create_app_store_version"), "{steps}");
    assert!(steps.contains("build_upload"), "{steps}");

    let requests = server.received_requests().await.unwrap();
    assert!(requests.len() <= MAX_REQUESTS);
    let builds = requests
        .iter()
        .find(|r| r.url.path() == "/v1/builds")
        .unwrap();
    let query: Vec<(String, String)> = builds.url.query_pairs().into_owned().collect();
    assert!(query.contains(&("limit".into(), "3".into())));
    assert!(query.contains(&("filter[preReleaseVersion.platform]".into(), "MAC_OS".into())));
}

#[tokio::test]
async fn pending_version_without_build_suggests_selecting_one() {
    let server = MockServer::start().await;
    mount(&server, &format!("/v1/apps/{APP}"), app_body()).await;
    mount(
        &server,
        &format!("/v1/apps/{APP}/appStoreVersions"),
        json!({"data": [version("v-next", "1.0", "PREPARE_FOR_SUBMISSION", "2026-11-01T00:00:00Z", None, None)]}),
    )
    .await;
    mount(
        &server,
        "/v1/reviewSubmissions",
        json!({"data": [{"type": "reviewSubmissions", "id": "rs-draft",
            "attributes": {"platform": "IOS", "state": "READY_FOR_REVIEW", "submittedDate": null},
            "relationships": {"items": {"data": []}}}]}),
    )
    .await;
    mount(
        &server,
        "/v1/builds",
        builds_body(vec![build(
            "b-1",
            "1",
            "VALID",
            "2026-11-02T00:00:00Z",
            "bd-1",
            "MISSING_EXPORT_COMPLIANCE",
            "MISSING_EXPORT_COMPLIANCE",
        )]),
    )
    .await;
    let ctx = context(&server);

    let out = call(&ctx, json!({"app_id": APP})).await.unwrap();

    assert!(out["live_version"].is_null());
    assert_eq!(out["pending_version"]["version_string"], "1.0");
    assert!(out["pending_version"]["build"].is_null());
    assert_eq!(out["review"]["state"], "READY_FOR_REVIEW");
    assert_eq!(out["review"]["items_count"], 0);
    let steps = out["next_steps"].to_string();
    assert!(steps.contains("has no build selected"), "{steps}");
    // Единственная сборка — версии 2.4.0: к версии 1.0 её привязать нельзя.
    assert!(steps.contains("none of the latest builds"), "{steps}");
    assert!(steps.contains("they have versions 2.4.0"), "{steps}");
    assert!(steps.contains("update_app_store_version"), "{steps}");
    assert!(steps.contains("set_app_store_version_build"), "{steps}");
    assert!(
        steps.contains("set_build_app_encryption_declaration (write-level)"),
        "{steps}"
    );
    // update_build может отозвать сборку: подсказка говорит, что нужен уровень удаления.
    assert!(
        steps.contains("update_build (destructive-level: needs destructive access"),
        "{steps}"
    );
    assert!(steps.contains("ITSAppUsesNonExemptEncryption"), "{steps}");
    assert!(steps.contains("draft review submission"), "{steps}");
    assert!(steps.contains("needs destructive access"), "{steps}");
}

#[tokio::test]
async fn pending_version_without_build_names_the_matching_build() {
    let server = MockServer::start().await;
    mount(&server, &format!("/v1/apps/{APP}"), app_body()).await;
    mount(
        &server,
        &format!("/v1/apps/{APP}/appStoreVersions"),
        json!({"data": [version("v-next", "2.4.0", "PREPARE_FOR_SUBMISSION", "2026-11-01T00:00:00Z", None, None)]}),
    )
    .await;
    mount(&server, "/v1/reviewSubmissions", empty_list()).await;
    mount(
        &server,
        "/v1/builds",
        builds_body(vec![build(
            "b-7",
            "7",
            "VALID",
            "2026-11-02T00:00:00Z",
            "bd-7",
            "IN_BETA_TESTING",
            "READY_FOR_BETA_SUBMISSION",
        )]),
    )
    .await;
    let ctx = context(&server);

    let out = call(&ctx, json!({"app_id": APP})).await.unwrap();

    let steps = out["next_steps"].to_string();
    assert!(steps.contains("attach build 7 (id b-7)"), "{steps}");
    assert!(!steps.contains("none of the latest builds"), "{steps}");
}

#[tokio::test]
async fn api_errors_become_tool_errors() {
    let server = MockServer::start().await;
    mount(&server, &format!("/v1/apps/{APP}"), app_body()).await;
    mount(
        &server,
        &format!("/v1/apps/{APP}/appStoreVersions"),
        empty_list(),
    )
    .await;
    mount(&server, "/v1/reviewSubmissions", empty_list()).await;
    Mock::given(method("GET"))
        .and(path("/v1/builds"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"errors": [{
            "status": "403", "code": "FORBIDDEN_ERROR",
            "title": "This request is forbidden for security reasons",
            "detail": "The API key in use does not allow this request"}]})))
        .mount(&server)
        .await;
    let ctx = context(&server);

    let err = call(&ctx, json!({"app_id": APP})).await.unwrap_err();
    assert!(
        err.message.contains("could not read builds"),
        "{}",
        err.message
    );
    assert!(err.message.contains("403"), "{}", err.message);
    assert!(server.received_requests().await.unwrap().len() <= MAX_REQUESTS);
}

#[tokio::test]
async fn invalid_arguments_are_rejected_without_requests() {
    let server = MockServer::start().await;
    let ctx = context(&server);
    for args in [
        json!({}),
        json!({"app_id": " "}),
        json!({"app_id": APP, "platform": "ANDROID"}),
        json!({"app_id": APP, "builds_limit": 0}),
    ] {
        assert!(call(&ctx, args.clone()).await.is_err(), "{args}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn registration() {
    let tools = composite::all();
    let tool = tools.iter().find(|t| t.name == NAME).unwrap();
    assert_eq!(tool.access, AccessLevel::Read);
    assert_eq!(tool.toolset, "versions-review");
    assert!(!tool.uses_local_files);
    assert!(appstore_connect_api::spec::find(NAME).is_none());
    let schema = (tool.input_schema)();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(["app_id"]));
    assert_eq!(
        schema["properties"]["platform"]["enum"],
        json!(["IOS", "MAC_OS", "TV_OS", "VISION_OS"])
    );
    assert!(release_status::REFERENCED_TOOLS.contains(&"set_app_store_version_build"));
}
