//! Тесты `report_download` на `wiremock` и временном каталоге. Отчёты синтетические,
//! gzip собирается в тесте; ключ ES256 генерируется в тесте, Key ID и Issuer ID фиктивные.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use appstore_connect_api::executor::{ApiClient, ClientConfig, RetryPolicy};
use appstore_connect_api::external::ExternalClient;
use appstore_connect_api::{
    AccessLevel, ApiKey, HttpMethod, Issuer, IssuerId, JwtTokens, KeyId, PrivateKeyPem,
};
use flate2::Compression;
use flate2::write::GzEncoder;
use p256::ecdsa::SigningKey;
use p256::elliptic_curve::Generate;
use p256::pkcs8::{EncodePrivateKey, LineEnding};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{header_regex, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::composite::testing::{DirFiles, MemoryVault};
use crate::composite::{Context, ProfileInfo};

const VENDOR: &str = "80012345";
const SIGNATURE: &str = "X-Amz-Signature=deadbeefsecret";

fn tokens() -> Arc<JwtTokens> {
    let pem = SigningKey::generate().to_pkcs8_pem(LineEnding::LF).unwrap();
    let key = ApiKey::new(
        &PrivateKeyPem::parse(&pem).unwrap(),
        KeyId::new("TESTKEY123").unwrap(),
        Issuer::Team(IssuerId::new("01234567-89ab-cdef-0123-456789abcdef").unwrap()),
    )
    .unwrap();
    Arc::new(JwtTokens::new(key))
}

struct Harness {
    server: MockServer,
    dir: TempDir,
    ctx: Context,
}

async fn harness(vendor: Option<&str>) -> Harness {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = ApiClient::new(ClientConfig {
        base_url: url::Url::parse(&server.uri()).unwrap(),
        tokens: Some(tokens()),
        timeout: Duration::from_secs(5),
        max_response_bytes: 64 * 1024,
        retry: RetryPolicy {
            max_retries: 0,
            default_delay: Duration::from_millis(10),
            max_delay: Duration::from_secs(1),
        },
    })
    .unwrap();
    let external = ExternalClient::new(Duration::from_secs(5), 64 * 1024)
        .unwrap()
        .allow_loopback_http();
    let ctx = Context {
        api: Arc::new(api),
        external: Arc::new(external),
        files: Arc::new(DirFiles::new(dir.path())),
        vault: Arc::new(MemoryVault::default()),
        profile: ProfileInfo {
            name: "test".into(),
            vendor_number: vendor.map(str::to_string),
        },
        access: AccessLevel::Read,
    };
    Harness { server, dir, ctx }
}

fn gzip(text: &str) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(text.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

fn gzip_response(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/a-gzip")
        .set_body_bytes(gzip(text))
}

fn apple_error(status: u16, code: &str, detail: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "errors": [{"status": status.to_string(), "code": code, "title": "error", "detail": detail}]
    }))
}

async fn call(ctx: &Context, args: Value) -> ToolResult {
    let tool = tools().into_iter().next().unwrap();
    (tool.run)(ctx, args).await
}

/// Файлы каталога, кроме скрытых: временные файлы должны быть удалены.
fn listing(dir: &TempDir) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

const SALES_TSV: &str = "Provider\tProvider Country\tSKU\tDeveloper\tTitle\tVersion\tProduct Type Identifier\tUnits\tDeveloper Proceeds\tBegin Date\tEnd Date\tCustomer Currency\tCountry Code\n\
APPLE\tUS\tcom.example.one\tExample\tOne\t1.0\t1\t10\t0\t10/01/2025\t10/31/2025\tUSD\tUS\n\
APPLE\tUS\tcom.example.one\tExample\tOne\t1.0\t1\t5\t0\t10/01/2025\t10/31/2025\tEUR\tDE\n\
APPLE\tUS\tcom.example.two\tExample\tTwo\t1.0\t1\t-1\t0\t10/01/2025\t10/31/2025\tUSD\tUS\n";

#[test]
fn registered_as_read_tool_with_local_files() {
    let tools = crate::composite::all();
    let tool = tools.iter().find(|t| t.name == NAME).unwrap();
    assert_eq!(tool.toolset, "analytics-reports");
    assert_eq!(tool.access, AccessLevel::Read);
    assert!(tool.uses_local_files);
    assert!(tool.name.len() <= 40);
    let schema = (tool.input_schema)();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(["kind", "save_to"]));
    assert!(
        appstore_connect_api::spec::operations()
            .iter()
            .all(|op| op.tool_name != NAME)
    );
}

#[test]
fn create_request_tool_exists_and_needs_write() {
    let op = appstore_connect_api::spec::find(CREATE_REQUEST_TOOL).unwrap();
    assert_eq!(op.method, HttpMethod::Post);
    assert_eq!(op.path, "/v1/analyticsReportRequests");
    assert_eq!(op.access, AccessLevel::Write);
}

/// Значения перечислений совпадают со спецификацией Apple.
#[test]
fn enums_match_the_spec() {
    let schema_of = |p: &str| -> Value {
        let op = appstore_connect_api::spec::operations()
            .iter()
            .find(|op| op.path == p && op.method == HttpMethod::Get)
            .unwrap();
        serde_json::from_str(op.input_schema).unwrap()
    };
    let enum_of = |schema: &Value, param: &str| -> Vec<String> {
        let props = schema["properties"].as_object().unwrap();
        let prop = props
            .iter()
            .find(|(name, _)| name.contains(param))
            .unwrap_or_else(|| panic!("{param} not in {props:?}"))
            .1;
        let values = prop
            .pointer("/items/enum")
            .or_else(|| prop.get("enum"))
            .unwrap_or_else(|| panic!("no enum for {param}: {prop}"));
        values
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    let strings = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();

    let sales = schema_of("/v1/salesReports");
    assert_eq!(enum_of(&sales, "reportType"), strings(SALES_REPORT_TYPES));
    assert_eq!(
        enum_of(&sales, "reportSubType"),
        strings(SALES_REPORT_SUB_TYPES)
    );
    assert_eq!(enum_of(&sales, "frequency"), strings(SALES_FREQUENCIES));
    let finance = schema_of("/v1/financeReports");
    assert_eq!(
        enum_of(&finance, "reportType"),
        strings(FINANCE_REPORT_TYPES)
    );
    let reports = schema_of("/v1/analyticsReportRequests/{id}/reports");
    assert_eq!(enum_of(&reports, "category"), strings(ANALYTICS_CATEGORIES));
    let instances = schema_of("/v1/analyticsReports/{id}/instances");
    assert_eq!(
        enum_of(&instances, "granularity"),
        strings(ANALYTICS_GRANULARITIES)
    );
    let requests = schema_of("/v1/apps/{id}/analyticsReportRequests");
    assert_eq!(
        enum_of(&requests, "accessType"),
        strings(ANALYTICS_ACCESS_TYPES)
    );
}

#[test]
fn dates_and_periods() {
    assert_eq!(Date::from_days(0).to_string(), "1970-01-01");
    let date = Date::parse("2024-02-29").unwrap();
    assert_eq!(Date::from_days(date.to_days()), date);
    assert_eq!(date.add_days(1).to_string(), "2024-03-01");
    assert!(Date::parse("2025-02-29").is_none());
    assert_eq!(normalize_date("10/31/2025").as_deref(), Some("2025-10-31"));
    assert_eq!(
        Period::parse("MONTHLY", "2024-02")
            .unwrap()
            .end()
            .to_string(),
        "2024-02-29"
    );
    assert!(Period::parse("MONTHLY", "2024-13").is_err());
    assert!(Period::parse("DAILY", "2024-10").is_err());
    assert_eq!(infer_frequency("2025").unwrap(), "YEARLY");
    assert_eq!(
        split_fields(br#"a,"b, c","d ""q""""#, b','),
        vec!["a", "b, c", r#"d "q""#]
    );
}

#[tokio::test]
async fn sales_monthly_report_is_saved_and_summarized() {
    let h = harness(Some(VENDOR)).await;
    Mock::given(method("GET"))
        .and(path("/v1/salesReports"))
        .and(query_param("filter[vendorNumber]", VENDOR))
        .and(query_param("filter[reportType]", "SALES"))
        .and(query_param("filter[reportSubType]", "SUMMARY"))
        .and(query_param("filter[frequency]", "MONTHLY"))
        .and(query_param("filter[reportDate]", "2025-10"))
        .and(query_param_is_missing("filter[version]"))
        .and(header_regex("accept", "^application/a-gzip"))
        .respond_with(gzip_response(SALES_TSV))
        .expect(1)
        .mount(&h.server)
        .await;

    let out = call(
        &h.ctx,
        json!({"kind": "sales", "save_to": "sales.tsv", "report_date": "2025-10"}),
    )
    .await
    .unwrap();

    assert_eq!(out["kind"], "sales");
    assert_eq!(out["rows"], 3);
    assert_eq!(out["columns"].as_array().unwrap().len(), 13);
    assert_eq!(out["columns"][2], "SKU");
    assert_eq!(out["report"]["frequency"], "MONTHLY");
    assert_eq!(out["summary"]["totals"]["Units"], 14);
    assert_eq!(out["summary"]["distinct"]["SKU"], 2);
    assert_eq!(out["summary"]["distinct"]["Country Code"], 2);
    assert_eq!(
        out["summary"]["date_range"],
        json!({"from": "2025-10-01", "to": "2025-10-31"})
    );
    assert!(
        out["summary"]["message"]
            .as_str()
            .unwrap()
            .contains("Total Units: 14")
    );

    let saved = std::fs::read_to_string(h.dir.path().join("sales.tsv")).unwrap();
    assert_eq!(saved, SALES_TSV);
    assert_eq!(out["bytes"], SALES_TSV.len());
    assert!(out["path"].as_str().unwrap().ends_with("sales.tsv"));
    // Содержимое отчёта в ответ не попадает.
    assert!(!out.to_string().contains("com.example.one"));
    assert_eq!(listing(&h.dir), vec!["sales.tsv"]);
}

#[tokio::test]
async fn vendor_number_argument_overrides_profile_and_is_required() {
    let h = harness(None).await;
    let err = call(
        &h.ctx,
        json!({"kind": "sales", "save_to": "s.tsv", "report_date": "2025-10-01"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("--vendor-number"), "{}", err.message);
    assert!(err.message.contains("--profile test"), "{}", err.message);

    Mock::given(method("GET"))
        .and(path("/v1/salesReports"))
        .and(query_param("filter[vendorNumber]", "85555555"))
        .and(query_param("filter[frequency]", "DAILY"))
        .and(query_param("filter[version]", "1_0"))
        .respond_with(gzip_response(SALES_TSV))
        .expect(1)
        .mount(&h.server)
        .await;
    let out = call(
        &h.ctx,
        json!({"kind": "sales", "save_to": "s.tsv", "report_date": "2025-10-01",
               "vendor_number": "85555555", "version": "1_0"}),
    )
    .await
    .unwrap();
    assert_eq!(out["rows"], 3);
}

#[tokio::test]
async fn finance_report_is_saved() {
    let h = harness(Some(VENDOR)).await;
    let tsv = "Start Date\tEnd Date\tUPC\tISRC/ISBN\tVendor Identifier\tQuantity\tPartner Share\tExtended Partner Share\tPartner Share Currency\tSales or Return\tApple Identifier\tArtist/Show/Developer/Author\tTitle\tLabel/Studio/Network/Developer/Publisher\tGrid\tProduct Type Identifier\tISAN/Other Identifier\tCountry Of Sale\n\
09/01/2025\t09/27/2025\t\t\tcom.example.one\t3\t0.70\t2.10\tUSD\tS\t1234567890\tExample\tOne\t\t\t1\t\tUS\n\
09/01/2025\t09/27/2025\t\t\tcom.example.one\t2\t0.65\t1.30\tEUR\tS\t1234567890\tExample\tOne\t\t\t1\t\tFR\n\
\n\
Total_Rows\t2\n\
Total_Amount\t3.40\n\
Total_Units\t5\n";
    Mock::given(method("GET"))
        .and(path("/v1/financeReports"))
        .and(query_param("filter[vendorNumber]", VENDOR))
        .and(query_param("filter[reportType]", "FINANCIAL"))
        .and(query_param("filter[regionCode]", "ZZ"))
        .and(query_param("filter[reportDate]", "2025-09"))
        .respond_with(gzip_response(tsv))
        .expect(1)
        .mount(&h.server)
        .await;
    let out = call(
        &h.ctx,
        json!({"kind": "finance", "save_to": "finance.tsv", "report_date": "2025-09"}),
    )
    .await
    .unwrap();
    assert_eq!(out["kind"], "finance");
    assert_eq!(out["report"]["region_code"], "ZZ");
    assert_eq!(out["rows"], 5);
    assert_eq!(out["summary"]["totals"]["Quantity"], 5);
    assert_eq!(out["summary"]["distinct"]["Country Of Sale"], 2);
    assert_eq!(out["summary"]["rows_not_in_totals"], 3);
    assert_eq!(
        out["summary"]["date_range"],
        json!({"from": "2025-09-01", "to": "2025-09-27"})
    );
    assert!(h.dir.path().join("finance.tsv").is_file());

    // FINANCE_DETAIL по умолчанию идёт с регионом Z1.
    Mock::given(method("GET"))
        .and(path("/v1/financeReports"))
        .and(query_param("filter[reportType]", "FINANCE_DETAIL"))
        .and(query_param("filter[regionCode]", "Z1"))
        .respond_with(gzip_response(tsv))
        .expect(1)
        .mount(&h.server)
        .await;
    call(
        &h.ctx,
        json!({"kind": "finance", "save_to": "detail.tsv", "report_date": "2025-09",
               "report_type": "FINANCE_DETAIL"}),
    )
    .await
    .unwrap();

    let err = call(&h.ctx, json!({"kind": "finance", "save_to": "x.tsv"}))
        .await
        .unwrap_err();
    assert!(err.message.contains("report_date"), "{}", err.message);
}

#[tokio::test]
async fn empty_report_404_is_explained_and_leaves_no_file() {
    let h = harness(Some(VENDOR)).await;
    Mock::given(method("GET"))
        .and(path("/v1/salesReports"))
        .respond_with(apple_error(
            404,
            "NOT_FOUND",
            "There were no sales for the date specified.",
        ))
        .mount(&h.server)
        .await;
    let err = call(
        &h.ctx,
        json!({"kind": "sales", "save_to": "empty.tsv", "report_date": "2025-10-05"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.starts_with("no data"), "{}", err.message);
    assert!(
        err.message.contains("There were no sales"),
        "{}",
        err.message
    );
    assert!(listing(&h.dir).is_empty(), "{:?}", listing(&h.dir));
}

#[tokio::test]
async fn report_not_available_yet_and_future_dates() {
    let h = harness(Some(VENDOR)).await;
    Mock::given(method("GET"))
        .and(path("/v1/salesReports"))
        .respond_with(apple_error(
            404,
            "NOT_FOUND",
            "Report is not available yet. Daily reports for the Americas are available by 5 am Pacific Time.",
        ))
        .mount(&h.server)
        .await;
    let err = call(
        &h.ctx,
        json!({"kind": "sales", "save_to": "a.tsv", "report_date": "2025-10-05"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("not ready yet"), "{}", err.message);

    // Будущий период не запрашивается вовсе.
    for args in [
        json!({"kind": "sales", "save_to": "b.tsv", "report_date": "2099-01-01"}),
        json!({"kind": "sales", "save_to": "b.tsv", "frequency": "YEARLY", "report_date": "2099"}),
        json!({"kind": "finance", "save_to": "b.tsv", "report_date": "2099-01"}),
    ] {
        let err = call(&h.ctx, args).await.unwrap_err();
        assert!(err.message.contains("not available yet"), "{}", err.message);
    }
    assert_eq!(h.server.received_requests().await.unwrap().len(), 1);
    assert!(listing(&h.dir).is_empty());
}

#[tokio::test]
async fn invalid_vendor_number_is_explained() {
    let h = harness(Some(VENDOR)).await;
    Mock::given(method("GET"))
        .and(path("/v1/salesReports"))
        .respond_with(apple_error(
            403,
            "FORBIDDEN_ERROR",
            "Invalid vendor number specified.",
        ))
        .mount(&h.server)
        .await;
    let err = call(
        &h.ctx,
        json!({"kind": "sales", "save_to": "v.tsv", "report_date": "2025-10-05"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("vendor number"), "{}", err.message);
    assert!(err.message.contains("--vendor-number"), "{}", err.message);

    let err = call(
        &h.ctx,
        json!({"kind": "sales", "save_to": "v.tsv", "vendor_number": "12ab"}),
    )
    .await
    .unwrap_err();
    assert!(
        err.message.contains("invalid vendor_number"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn existing_save_to_is_refused_before_any_request() {
    let h = harness(Some(VENDOR)).await;
    std::fs::write(h.dir.path().join("taken.tsv"), "keep me").unwrap();
    let err = call(
        &h.ctx,
        json!({"kind": "sales", "save_to": "taken.tsv", "report_date": "2025-10"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("already exists"), "{}", err.message);
    assert_eq!(
        std::fs::read_to_string(h.dir.path().join("taken.tsv")).unwrap(),
        "keep me"
    );
    assert!(h.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn arguments_of_another_kind_are_rejected() {
    let h = harness(Some(VENDOR)).await;
    let err = call(
        &h.ctx,
        json!({"kind": "sales", "save_to": "x.tsv", "app_id": "1"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("app_id"), "{}", err.message);
    let err = call(
        &h.ctx,
        json!({"kind": "analytics", "save_to": "x.csv", "app_id": "1", "report_date": "2025-10"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("report_date"), "{}", err.message);
}

// ---------------------------------------------------------------------------------------
// Аналитика
// ---------------------------------------------------------------------------------------

const APP: &str = "1234567890";

async fn mount_requests(server: &MockServer, data: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/v1/apps/{APP}/analyticsReportRequests")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": data})))
        .mount(server)
        .await;
}

async fn mount_reports(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/analyticsReportRequests/req-ongoing/reports"))
        .and(query_param("filter[name]", "App Downloads Standard"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"type": "analyticsReports", "id": "rep-1",
             "attributes": {"name": "App Downloads Standard", "category": "COMMERCE"}}
        ]})))
        .mount(server)
        .await;
}

fn requests_data() -> Value {
    json!([
        {"type": "analyticsReportRequests", "id": "req-snapshot",
         "attributes": {"accessType": "ONE_TIME_SNAPSHOT", "stoppedDueToInactivity": false}},
        {"type": "analyticsReportRequests", "id": "req-ongoing",
         "attributes": {"accessType": "ONGOING", "stoppedDueToInactivity": false}}
    ])
}

#[tokio::test]
async fn analytics_without_request_explains_how_to_create_one() {
    let h = harness(None).await;
    mount_requests(&h.server, json!([])).await;
    let err = call(
        &h.ctx,
        json!({"kind": "analytics", "save_to": "a.csv", "app_id": APP,
               "report_name": "App Downloads Standard"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains(CREATE_REQUEST_TOOL), "{}", err.message);
    assert!(err.message.contains("--allow-write"), "{}", err.message);
    assert!(err.message.contains("1-2 days"), "{}", err.message);
    assert!(err.message.contains(APP), "{}", err.message);
    // Только чтение: ни одного POST.
    let requests = h.server.received_requests().await.unwrap();
    assert!(requests.iter().all(|r| r.method.as_str() == "GET"));
    assert!(listing(&h.dir).is_empty());
}

#[tokio::test]
async fn analytics_joins_segments_with_one_header() {
    let h = harness(None).await;
    mount_requests(&h.server, requests_data()).await;
    mount_reports(&h.server).await;
    Mock::given(method("GET"))
        .and(path("/v1/analyticsReports/rep-1/instances"))
        .and(query_param("filter[granularity]", "DAILY"))
        .and(query_param("filter[processingDate]", "2025-10-14"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"type": "analyticsReportInstances", "id": "inst-1",
             "attributes": {"granularity": "DAILY", "processingDate": "2025-10-14"}}
        ]})))
        .mount(&h.server)
        .await;
    let base = h.server.uri();
    Mock::given(method("GET"))
        .and(path("/v1/analyticsReportInstances/inst-1/segments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"type": "analyticsReportSegments", "id": "seg-1",
             "attributes": {"checksum": "abc", "sizeInBytes": 100,
                            "url": format!("{base}/signed/seg-1.csv.gz?{SIGNATURE}")}},
            {"type": "analyticsReportSegments", "id": "seg-2",
             "attributes": {"checksum": "def", "sizeInBytes": 100,
                            "url": format!("{base}/signed/seg-2.csv.gz?{SIGNATURE}")}}
        ]})))
        .mount(&h.server)
        .await;
    let header = "Date\tApp Name\tApp Apple Identifier\tDownload Type\tTerritory\tCounts";
    let seg1 = format!(
        "{header}\n2025-10-13\tOne\t{APP}\tFirst-time download\tUS\t7\n2025-10-13\tOne\t{APP}\tRedownload\tDE\t2\n"
    );
    let seg2 = format!("{header}\r\n2025-10-14\tOne\t{APP}\tFirst-time download\tFR\t4\r\n");
    Mock::given(method("GET"))
        .and(path("/signed/seg-1.csv.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(gzip(&seg1)))
        .expect(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/signed/seg-2.csv.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(gzip(&seg2)))
        .expect(1)
        .mount(&h.server)
        .await;

    let out = call(
        &h.ctx,
        json!({"kind": "analytics", "save_to": "downloads.tsv", "app_id": APP,
               "report_name": "App Downloads Standard", "processing_date": "2025-10-14"}),
    )
    .await
    .unwrap();

    let saved = std::fs::read_to_string(h.dir.path().join("downloads.tsv")).unwrap();
    assert_eq!(saved.matches("Date\tApp Name").count(), 1, "{saved}");
    assert_eq!(saved.lines().count(), 4, "{saved}");
    assert!(!saved.contains('\r'));
    assert_eq!(out["rows"], 3);
    assert_eq!(out["columns"][5], "Counts");
    assert_eq!(out["report"]["access_type"], "ONGOING");
    assert_eq!(out["report"]["report_request_id"], "req-ongoing");
    assert_eq!(out["report"]["category"], "COMMERCE");
    assert_eq!(out["summary"]["instances"], 1);
    assert_eq!(out["summary"]["segments"], 2);
    assert_eq!(out["summary"]["totals"]["Counts"], 13);
    assert_eq!(out["summary"]["distinct"]["Territory"], 3);
    assert_eq!(
        out["summary"]["date_range"],
        json!({"from": "2025-10-13", "to": "2025-10-14"})
    );
    // Подписанные ссылки не попадают в ответ.
    let text = out.to_string();
    assert!(!text.contains("signed"), "{text}");
    assert!(!text.contains(SIGNATURE), "{text}");
    assert!(!text.contains("http"), "{text}");
    assert_eq!(listing(&h.dir), vec!["downloads.tsv"]);
}

#[tokio::test]
async fn analytics_segment_download_error_hides_signed_url() {
    let h = harness(None).await;
    mount_requests(&h.server, requests_data()).await;
    mount_reports(&h.server).await;
    Mock::given(method("GET"))
        .and(path("/v1/analyticsReports/rep-1/instances"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"type": "analyticsReportInstances", "id": "inst-1",
             "attributes": {"granularity": "DAILY", "processingDate": "2025-10-14"}}
        ]})))
        .mount(&h.server)
        .await;
    let url = format!("{}/signed/seg-1.csv.gz?{SIGNATURE}", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/v1/analyticsReportInstances/inst-1/segments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"type": "analyticsReportSegments", "id": "seg-1", "attributes": {"url": url}}
        ]})))
        .mount(&h.server)
        .await;
    // Хранилище отражает адрес с подписью в теле ошибки.
    Mock::given(method("GET"))
        .and(path("/signed/seg-1.csv.gz"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string(format!("<Error>expired {url}</Error>")),
        )
        .mount(&h.server)
        .await;
    let err = call(
        &h.ctx,
        json!({"kind": "analytics", "save_to": "a.tsv", "app_id": APP,
               "report_name": "App Downloads Standard"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("segment 1 of 1"), "{}", err.message);
    assert!(!err.message.contains(SIGNATURE), "{}", err.message);
    assert!(!err.message.contains("deadbeef"), "{}", err.message);
    assert!(listing(&h.dir).is_empty(), "{:?}", listing(&h.dir));
}

#[tokio::test]
async fn analytics_instance_not_ready() {
    let h = harness(None).await;
    mount_requests(&h.server, requests_data()).await;
    mount_reports(&h.server).await;
    // Экземпляра за запрошенную дату нет.
    Mock::given(method("GET"))
        .and(path("/v1/analyticsReports/rep-1/instances"))
        .and(query_param("filter[processingDate]", "2099-01-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&h.server)
        .await;
    let err = call(
        &h.ctx,
        json!({"kind": "analytics", "save_to": "a.tsv", "app_id": APP,
               "report_name": "App Downloads Standard", "processing_date": "2099-01-01"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("not ready yet"), "{}", err.message);

    // Экземпляр есть, но сегментов ещё нет.
    Mock::given(method("GET"))
        .and(path("/v1/analyticsReports/rep-1/instances"))
        .and(query_param_is_missing("filter[processingDate]"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"type": "analyticsReportInstances", "id": "inst-old",
             "attributes": {"granularity": "DAILY", "processingDate": "2025-10-01"}},
            {"type": "analyticsReportInstances", "id": "inst-new",
             "attributes": {"granularity": "DAILY", "processingDate": "2025-10-02"}}
        ]})))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/analyticsReportInstances/inst-new/segments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(1)
        .mount(&h.server)
        .await;
    let err = call(
        &h.ctx,
        json!({"kind": "analytics", "save_to": "a.tsv", "app_id": APP,
               "report_name": "App Downloads Standard"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("not ready yet"), "{}", err.message);
    assert!(err.message.contains("2025-10-02"), "{}", err.message);
    assert!(listing(&h.dir).is_empty());
}

#[tokio::test]
async fn analytics_unknown_report_name_lists_available_names() {
    let h = harness(None).await;
    mount_requests(&h.server, requests_data()).await;
    Mock::given(method("GET"))
        .and(path("/v1/analyticsReportRequests/req-ongoing/reports"))
        .and(query_param("filter[name]", "Downloads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/analyticsReportRequests/req-ongoing/reports"))
        .and(query_param_is_missing("filter[name]"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"type": "analyticsReports", "id": "rep-1",
             "attributes": {"name": "App Downloads Standard", "category": "COMMERCE"}},
            {"type": "analyticsReports", "id": "rep-2",
             "attributes": {"name": "App Downloads Detailed", "category": "COMMERCE"}}
        ]})))
        .mount(&h.server)
        .await;
    let err = call(
        &h.ctx,
        json!({"kind": "analytics", "save_to": "a.tsv", "app_id": APP,
               "report_name": "Downloads"}),
    )
    .await
    .unwrap_err();
    assert!(
        err.message.contains("\"App Downloads Standard\""),
        "{}",
        err.message
    );

    // Категория с несколькими отчётами требует report_name.
    let err = call(
        &h.ctx,
        json!({"kind": "analytics", "save_to": "a.tsv", "app_id": APP, "category": "COMMERCE"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("pass report_name"), "{}", err.message);
}

#[test]
fn request_preference() {
    let data = requests_data();
    let items = data.as_array().unwrap();
    assert_eq!(pick_request(items, None).unwrap().0, "req-ongoing");
    assert_eq!(
        pick_request(items, Some("ONE_TIME_SNAPSHOT")).unwrap().0,
        "req-snapshot"
    );
    let stopped = json!([
        {"id": "a", "attributes": {"accessType": "ONGOING", "stoppedDueToInactivity": true}},
        {"id": "b", "attributes": {"accessType": "ONE_TIME_SNAPSHOT"}}
    ]);
    assert_eq!(
        pick_request(stopped.as_array().unwrap(), None).unwrap().0,
        "b"
    );
    assert!(pick_request(&[], None).is_none());
}
