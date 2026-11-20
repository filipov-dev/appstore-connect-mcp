//! Составной инструмент `report_download`: отчёты продаж, финансов и аналитики в локальный
//! файл.
//!
//! - Продажи и финансы: `GET /v1/salesReports` / `GET /v1/financeReports` с фильтрами
//!   `filter[...]`. App Store Connect отдаёт gzip (`application/a-gzip`); исполнитель пишет его
//!   во временный файл рядом с `save_to`, затем файл потоково распаковывается в `save_to`.
//! - Аналитика: существующий запрос отчётов приложения (`analyticsReportRequests`, сначала
//!   `ONGOING`) → отчёт по имени или категории → экземпляры по гранулярности и дате →
//!   сегменты → каждый сегмент по подписанной ссылке через [`Context::external`] во временный
//!   файл → распаковка и склейка CSV/TSV с одним заголовком в `save_to`.
//!
//! Инструмент только читает: запрос отчётов аналитики (`POST /v1/analyticsReportRequests`) —
//! операция записи, её агент выполняет отдельным сгенерированным инструментом.
//!
//! В ответ не попадают ни содержимое отчёта, ни подписанные ссылки сегментов: только путь,
//! размер, число строк, колонки и короткая сводка. Распакованный отчёт не держится в памяти:
//! строки идут из распаковщика прямо в файл, в памяти — только агрегаты сводки.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use appstore_connect_api::executor::{
    ApiError, ApiRequest, ApiResponse, HttpError, ResponseBody, render_path,
};
use appstore_connect_api::{AccessLevel, HttpMethod, ResponseKind};
use flate2::read::MultiGzDecoder;
use serde_json::{Map, Value, json};

use super::{CompositeTool, Context, ToolError, ToolFuture, ToolResult};

#[cfg(test)]
mod tests;

/// Имя инструмента.
pub const NAME: &str = "report_download";

/// Сгенерированный инструмент `POST /v1/analyticsReportRequests` (уровень записи).
/// Тест сверяет имя с таблицей операций.
const CREATE_REQUEST_TOOL: &str = "create_analytics_report_request";

/// Значения перечислений — из спецификации Apple (`filter[...]` операций отчётов);
/// тест сверяет их со снимком спецификации.
const SALES_REPORT_TYPES: &[&str] = &[
    "SALES",
    "PRE_ORDER",
    "NEWSSTAND",
    "SUBSCRIPTION",
    "SUBSCRIPTION_EVENT",
    "SUBSCRIBER",
    "SUBSCRIPTION_OFFER_CODE_REDEMPTION",
    "INSTALLS",
    "FIRST_ANNUAL",
    "WIN_BACK_ELIGIBILITY",
];
const SALES_REPORT_SUB_TYPES: &[&str] = &[
    "SUMMARY",
    "DETAILED",
    "SUMMARY_INSTALL_TYPE",
    "SUMMARY_TERRITORY",
    "SUMMARY_CHANNEL",
];
const SALES_FREQUENCIES: &[&str] = &["DAILY", "WEEKLY", "MONTHLY", "YEARLY"];
const FINANCE_REPORT_TYPES: &[&str] = &["FINANCIAL", "FINANCE_DETAIL"];
const ANALYTICS_CATEGORIES: &[&str] = &[
    "APP_USAGE",
    "APP_STORE_ENGAGEMENT",
    "COMMERCE",
    "FRAMEWORK_USAGE",
    "PERFORMANCE",
];
const ANALYTICS_GRANULARITIES: &[&str] = &["DAILY", "WEEKLY", "MONTHLY"];
const ANALYTICS_ACCESS_TYPES: &[&str] = &["ONE_TIME_SNAPSHOT", "ONGOING"];

/// Аргументы каждого вида отчёта: остальные отклоняются, чтобы агент не думал, что
/// они на что-то повлияли.
const COMMON_ARGS: &[&str] = &["kind", "save_to"];
const SALES_ARGS: &[&str] = &[
    "vendor_number",
    "report_type",
    "report_sub_type",
    "frequency",
    "report_date",
    "version",
];
const FINANCE_ARGS: &[&str] = &["vendor_number", "report_type", "region_code", "report_date"];
const ANALYTICS_ARGS: &[&str] = &[
    "app_id",
    "report_name",
    "category",
    "granularity",
    "processing_date",
    "date_from",
    "date_to",
    "access_type",
];

/// Лимит размера страницы списков аналитики (максимум спецификации).
const PAGE_LIMIT: &str = "200";
/// Сколько записей списка читать по страницам, не больше.
const MAX_LIST_ITEMS: usize = 5000;
/// Сколько экземпляров отчёта аналитики склеивать за один вызов.
const MAX_INSTANCES: usize = 100;
/// Сколько имён отчётов перечислять в подсказке.
const MAX_LISTED_NAMES: usize = 40;
/// Предел распакованного отчёта: защита диска от gzip-бомбы.
const MAX_REPORT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Сколько различных значений колонки считать точно; дальше сводка пишет «не меньше».
const MAX_DISTINCT: usize = 100_000;
/// Окончание имени временного файла со сжатым отчётом или сегментом рядом с `save_to`.
/// Сжатые данные идут во временный файл, а не в память: лимит размера ответа исполнителя
/// на них не распространяется.
const TEMP_SUFFIX: &str = "download.gz";

const DESCRIPTION: &str = "Download an App Store Connect report into a new local file and \
return a short summary (rows, columns, date range, totals) instead of the content. \
kind=sales: Sales and Trends report (GET /v1/salesReports); needs frequency or report_date \
(DAILY/WEEKLY: YYYY-MM-DD, MONTHLY: YYYY-MM, YEARLY: YYYY; frequency is inferred from the \
date format when omitted); report_type defaults to SALES, report_sub_type to SUMMARY; pass \
version only if Apple requires a specific report version (for example 1_0). kind=finance: \
financial report (GET /v1/financeReports) for a fiscal month report_date=YYYY-MM; \
report_type defaults to FINANCIAL with region_code ZZ (all regions), FINANCE_DETAIL uses \
region_code Z1. Sales and finance use the vendor number of the profile unless vendor_number \
is passed. kind=analytics: App Analytics report of app_id: finds the existing report request \
of the app (ONGOING first), the report by report_name (exact name, for example \"App \
Downloads Standard\") or category, its instances by granularity (default DAILY) and \
processing_date (YYYY-MM-DD) or date_from/date_to (default: the latest instance), and joins \
all segments into one file with a single header row. This tool never creates report \
requests: if the app has none, create one with create_analytics_report_request (write \
access) and wait 1-2 days for Apple to generate data. The file is gzip-decompressed TSV/CSV \
as Apple provides it; read it locally instead of asking for its content. save_to must be a \
new file: existing files are not overwritten. Signed segment URLs are never returned.";

/// Описание инструмента для реестра составных инструментов.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: "analytics-reports",
        access: AccessLevel::Read,
        description: DESCRIPTION,
        input_schema,
        uses_local_files: true,
        run,
    }]
}

fn input_schema() -> Value {
    let report_types: Vec<&str> = SALES_REPORT_TYPES
        .iter()
        .chain(FINANCE_REPORT_TYPES)
        .copied()
        .collect();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["kind", "save_to"],
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["sales", "finance", "analytics"],
                "description": "Report family: sales (Sales and Trends), finance (financial reports) or analytics (App Analytics reports)."
            },
            "save_to": {
                "type": "string",
                "description": "Local path of the new file for the decompressed report (for example reports/sales-2026-10.tsv). The file must not exist."
            },
            "vendor_number": {
                "type": "string",
                "description": "sales, finance: vendor number from Payments and Financial Reports. Default: the vendor number of the profile."
            },
            "report_type": {
                "type": "string",
                "enum": report_types,
                "description": "sales: SALES (default), PRE_ORDER, NEWSSTAND, SUBSCRIPTION, SUBSCRIPTION_EVENT, SUBSCRIBER, SUBSCRIPTION_OFFER_CODE_REDEMPTION, INSTALLS, FIRST_ANNUAL, WIN_BACK_ELIGIBILITY. finance: FINANCIAL (default) or FINANCE_DETAIL."
            },
            "report_sub_type": {
                "type": "string",
                "enum": SALES_REPORT_SUB_TYPES,
                "description": "sales: report sub-type. Default: SUMMARY."
            },
            "frequency": {
                "type": "string",
                "enum": SALES_FREQUENCIES,
                "description": "sales: report period. Default: inferred from report_date (YYYY-MM-DD: DAILY, YYYY-MM: MONTHLY, YYYY: YEARLY), DAILY without report_date."
            },
            "report_date": {
                "type": "string",
                "description": "sales: DAILY and WEEKLY YYYY-MM-DD (WEEKLY: the last day of the week), MONTHLY YYYY-MM, YEARLY YYYY; omitted: the latest available report. finance: fiscal month YYYY-MM (required)."
            },
            "version": {
                "type": "string",
                "description": "sales: report version required by Apple for some report types, for example 1_0 or 1_3. Default: not sent."
            },
            "region_code": {
                "type": "string",
                "description": "finance: region code, for example ZZ (all regions, default for FINANCIAL), Z1 (default for FINANCE_DETAIL), US, EU, JP."
            },
            "app_id": {
                "type": "string",
                "description": "analytics: App Store Connect app ID (required)."
            },
            "report_name": {
                "type": "string",
                "description": "analytics: exact report name, for example \"App Downloads Standard\" or \"App Store Discovery and Engagement Detailed\"."
            },
            "category": {
                "type": "string",
                "enum": ANALYTICS_CATEGORIES,
                "description": "analytics: report category; enough on its own when the category has a single report, otherwise also pass report_name."
            },
            "granularity": {
                "type": "string",
                "enum": ANALYTICS_GRANULARITIES,
                "description": "analytics: instance granularity. Default: DAILY."
            },
            "processing_date": {
                "type": "string",
                "description": "analytics: processing date of the instance, YYYY-MM-DD. Default: the latest instance."
            },
            "date_from": {
                "type": "string",
                "description": "analytics: first processing date to include, YYYY-MM-DD (instead of processing_date)."
            },
            "date_to": {
                "type": "string",
                "description": "analytics: last processing date to include, YYYY-MM-DD (instead of processing_date)."
            },
            "access_type": {
                "type": "string",
                "enum": ANALYTICS_ACCESS_TYPES,
                "description": "analytics: use the report request of this access type. Default: ONGOING if the app has one, otherwise ONE_TIME_SNAPSHOT."
            }
        }
    })
}

fn run<'a>(ctx: &'a Context, args: Value) -> ToolFuture<'a> {
    Box::pin(download(ctx, args))
}

async fn download(ctx: &Context, args: Value) -> ToolResult {
    let kind = required_str(&args, "kind")?;
    let allowed = match kind {
        "sales" => SALES_ARGS,
        "finance" => FINANCE_ARGS,
        "analytics" => ANALYTICS_ARGS,
        other => {
            return Err(ToolError::new(format!(
                "unknown kind `{other}`: use sales, finance or analytics"
            )));
        }
    };
    check_args(&args, kind, allowed)?;
    let save_to = required_str(&args, "save_to")?;
    match kind {
        "sales" => {
            let plan = sales_plan(ctx, &args)?;
            download_api_report(ctx, plan, save_to).await
        }
        "finance" => {
            let plan = finance_plan(ctx, &args)?;
            download_api_report(ctx, plan, save_to).await
        }
        _ => download_analytics(ctx, &args, save_to).await,
    }
}

/// Аргументы, которые не относятся к виду отчёта, — ошибка с подсказкой.
fn check_args(args: &Value, kind: &str, allowed: &[&str]) -> Result<(), ToolError> {
    let Some(object) = args.as_object() else {
        return Err(ToolError::new("arguments must be a JSON object"));
    };
    let extra: Vec<&str> = object
        .iter()
        .filter(|(key, value)| {
            !value.is_null()
                && !COMMON_ARGS.contains(&key.as_str())
                && !allowed.contains(&key.as_str())
        })
        .map(|(key, _)| key.as_str())
        .collect();
    if extra.is_empty() {
        Ok(())
    } else {
        Err(ToolError::new(format!(
            "argument(s) {} do not apply to kind={kind}; allowed: {}",
            extra.join(", "),
            allowed.join(", ")
        )))
    }
}

// ---------------------------------------------------------------------------------------
// Продажи и финансы
// ---------------------------------------------------------------------------------------

/// Запрос отчёта продаж или финансов, уже проверенный.
#[derive(Debug)]
struct ApiReportPlan {
    kind: &'static str,
    path: &'static str,
    query: Vec<(String, String)>,
    /// Период отчёта (для проверки «ещё не готов» и сообщений); `None` — последний доступный.
    period: Option<Period>,
    report_date: Option<String>,
    /// Параметры отчёта для ответа агенту.
    report: Map<String, Value>,
}

fn sales_plan(ctx: &Context, args: &Value) -> Result<ApiReportPlan, ToolError> {
    let vendor = vendor_number(ctx, args)?;
    let report_type =
        enum_arg(args, "report_type", SALES_REPORT_TYPES, "kind=sales")?.unwrap_or("SALES");
    let sub_type = enum_arg(
        args,
        "report_sub_type",
        SALES_REPORT_SUB_TYPES,
        "kind=sales",
    )?
    .unwrap_or("SUMMARY");
    let report_date = optional_str(args, "report_date")?;
    let frequency = match enum_arg(args, "frequency", SALES_FREQUENCIES, "kind=sales")? {
        Some(frequency) => frequency,
        None => match report_date {
            None => "DAILY",
            Some(date) => infer_frequency(date)?,
        },
    };
    let period = report_date
        .map(|date| Period::parse(frequency, date))
        .transpose()?;
    let version = optional_str(args, "version")?;
    if let Some(version) = version
        && !is_report_version(version)
    {
        return Err(ToolError::new(format!(
            "invalid version `{version}`: expected digits with an underscore, for example 1_0"
        )));
    }

    let mut query = vec![
        ("filter[vendorNumber]".to_string(), vendor.clone()),
        ("filter[reportType]".to_string(), report_type.to_string()),
        ("filter[reportSubType]".to_string(), sub_type.to_string()),
        ("filter[frequency]".to_string(), frequency.to_string()),
    ];
    if let Some(date) = report_date {
        query.push(("filter[reportDate]".to_string(), date.to_string()));
    }
    if let Some(version) = version {
        query.push(("filter[version]".to_string(), version.to_string()));
    }
    let mut report = Map::new();
    report.insert("report_type".into(), json!(report_type));
    report.insert("report_sub_type".into(), json!(sub_type));
    report.insert("frequency".into(), json!(frequency));
    if let Some(date) = report_date {
        report.insert("report_date".into(), json!(date));
    }
    if let Some(version) = version {
        report.insert("version".into(), json!(version));
    }
    Ok(ApiReportPlan {
        kind: "sales",
        path: "/v1/salesReports",
        query,
        period,
        report_date: report_date.map(str::to_string),
        report,
    })
}

fn finance_plan(ctx: &Context, args: &Value) -> Result<ApiReportPlan, ToolError> {
    let vendor = vendor_number(ctx, args)?;
    let report_type =
        enum_arg(args, "report_type", FINANCE_REPORT_TYPES, "kind=finance")?.unwrap_or("FINANCIAL");
    let region = match optional_str(args, "region_code")? {
        Some(region) => {
            let region = region.trim();
            if region.is_empty()
                || region.len() > 3
                || !region.bytes().all(|b| b.is_ascii_alphanumeric())
            {
                return Err(ToolError::new(format!(
                    "invalid region_code `{region}`: expected a short code such as ZZ, Z1, US or EU"
                )));
            }
            region.to_ascii_uppercase()
        }
        // Сводный отчёт по всем регионам: ZZ для FINANCIAL, Z1 для FINANCE_DETAIL.
        None if report_type == "FINANCE_DETAIL" => "Z1".to_string(),
        None => "ZZ".to_string(),
    };
    let Some(report_date) = optional_str(args, "report_date")? else {
        return Err(ToolError::new(
            "kind=finance needs report_date: the fiscal month as YYYY-MM, for example 2026-09",
        ));
    };
    let period = Period::parse("MONTHLY", report_date).map_err(|_| {
        ToolError::new(format!(
            "invalid report_date `{report_date}` for kind=finance: expected the fiscal month as YYYY-MM"
        ))
    })?;
    let query = vec![
        ("filter[vendorNumber]".to_string(), vendor),
        ("filter[reportType]".to_string(), report_type.to_string()),
        ("filter[regionCode]".to_string(), region.clone()),
        ("filter[reportDate]".to_string(), report_date.to_string()),
    ];
    let mut report = Map::new();
    report.insert("report_type".into(), json!(report_type));
    report.insert("region_code".into(), json!(region));
    report.insert("report_date".into(), json!(report_date));
    Ok(ApiReportPlan {
        kind: "finance",
        path: "/v1/financeReports",
        query,
        period: Some(period),
        report_date: Some(report_date.to_string()),
        report,
    })
}

/// Vendor number: из аргумента или профиля.
fn vendor_number(ctx: &Context, args: &Value) -> Result<String, ToolError> {
    let value = match optional_str(args, "vendor_number")? {
        Some(value) => value.trim().to_string(),
        None => match ctx.profile.vendor_number.as_deref().map(str::trim) {
            Some(value) if !value.is_empty() => value.to_string(),
            _ => {
                return Err(ToolError::new(format!(
                    "no vendor number: sales and finance reports need the vendor number shown in \
                     App Store Connect > Payments and Financial Reports. Pass vendor_number, or ask \
                     the user to save it in the profile: appstore-connect-mcp auth login --profile {} \
                     --vendor-number <NUMBER>",
                    profile_name(ctx)
                )));
            }
        },
    };
    if value.is_empty() || value.len() > 32 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ToolError::new(
            "invalid vendor_number: expected the digits shown in App Store Connect > Payments and Financial Reports",
        ));
    }
    Ok(value)
}

fn profile_name(ctx: &Context) -> &str {
    if ctx.profile.name.is_empty() {
        "<PROFILE>"
    } else {
        &ctx.profile.name
    }
}

async fn download_api_report(ctx: &Context, plan: ApiReportPlan, save_to: &str) -> ToolResult {
    let target = Target::resolve(ctx, save_to)?;
    if let Some(period) = &plan.period {
        let today = Date::today();
        if period.end() >= today {
            return Err(ToolError::new(format!(
                "the {} report for {} is not available yet: the period ends on {} and Apple \
                 publishes reports only after the period is over (daily sales reports by about \
                 5 am Pacific Time the next day, monthly ones a few days after the month ends, \
                 finance reports after the fiscal month closes). Choose an earlier report_date \
                 or try again later.",
                plan.kind,
                plan.report_date.as_deref().unwrap_or("this period"),
                period.end()
            )));
        }
    }

    let temp = TempFile::new(&target, 0);
    let mut request = ApiRequest::new(HttpMethod::Get, plan.path);
    request.query = plan.query.clone();
    request.response = ResponseKind::Binary;
    request.save_to = Some(temp.path.clone());
    let response = ctx
        .api
        .execute(request)
        .await
        .map_err(|err| api_report_error(err, &plan, ctx))?;
    if !matches!(response.body, ResponseBody::SavedToFile { .. }) {
        return Err(ToolError::new(format!(
            "App Store Connect returned no {} report file",
            plan.kind
        )));
    }

    let mut table = TableWriter::create(&target)?;
    table = append_blocking(table, temp.path.clone(), "the report".to_string()).await?;
    drop(temp);
    let done = table.finish()?;

    let mut summary = done.stats.summary();
    let message = if done.rows == 0 {
        format!(
            "The {} report is empty: the file has no data rows.",
            plan.kind
        )
    } else {
        describe(plan.kind, &done, &summary)
    };
    summary.insert("message".into(), json!(message));
    Ok(json!({
        "path": done.path.display().to_string(),
        "kind": plan.kind,
        "rows": done.rows,
        "columns": done.columns,
        "bytes": done.bytes,
        "report": Value::Object(plan.report),
        "summary": Value::Object(summary),
    }))
}

/// Ошибка API отчёта продаж или финансов — понятным агенту текстом.
fn api_report_error(err: ApiError, plan: &ApiReportPlan, ctx: &Context) -> ToolError {
    let ApiError::Http(http) = &err else {
        return err.into();
    };
    let apple = apple_detail(http);
    let mentions = |needle: &str| apple.to_ascii_lowercase().contains(needle);
    if mentions("vendor") {
        return ToolError::new(format!(
            "App Store Connect rejected the vendor number (HTTP {}: {apple}). Check the vendor \
             number in App Store Connect > Payments and Financial Reports and pass vendor_number, \
             or save the right one in the profile: appstore-connect-mcp auth login --profile {} \
             --vendor-number <NUMBER>.",
            http.status,
            profile_name(ctx)
        ));
    }
    match http.status {
        404 if mentions("not available yet") || mentions("not yet available") => {
            ToolError::new(format!(
                "the {} report for {} is not ready yet (Apple: {apple}). Try again later or \
                 choose an earlier report_date.",
                plan.kind,
                plan.report_date.as_deref().unwrap_or("the latest period")
            ))
        }
        404 => ToolError::new(format!(
            "no data: Apple has no {} report for {} with these filters (Apple: {apple}). Either \
             there were no {} in that period, or the report is not generated yet; check \
             report_type, report_sub_type and the date, or choose another period. No file was \
             written.",
            plan.kind,
            plan.report_date.as_deref().unwrap_or("the latest period"),
            if plan.kind == "sales" {
                "sales or downloads"
            } else {
                "payments"
            }
        )),
        403 => ToolError::new(format!(
            "App Store Connect denied access to {} reports (Apple: {apple}). The API key needs \
             the Sales and Reports or Finance role (or Admin), and the vendor number must belong \
             to this account: check vendor_number or the vendor number of the profile \
             (appstore-connect-mcp auth login --profile {} --vendor-number <NUMBER>).",
            plan.kind,
            profile_name(ctx)
        )),
        _ => err.into(),
    }
}

/// Текст ошибок JSON:API без префикса HTTP: `detail` (или `title`) каждой ошибки.
fn apple_detail(http: &HttpError) -> String {
    let parts: Vec<String> = http
        .errors
        .iter()
        .filter_map(|e| {
            let text = e.detail.as_deref().or(e.title.as_deref())?;
            Some(match e.code.as_deref() {
                Some(code) => format!("{code}: {text}"),
                None => text.to_string(),
            })
        })
        .collect();
    if parts.is_empty() {
        let body = http.body.trim();
        if body.is_empty() {
            format!("HTTP {}", http.status)
        } else {
            body.chars().take(300).collect()
        }
    } else {
        parts.join("; ")
    }
}

// ---------------------------------------------------------------------------------------
// Аналитика
// ---------------------------------------------------------------------------------------

/// Выбор экземпляров отчёта аналитики.
#[derive(Debug)]
enum InstanceFilter {
    Latest,
    Date(Date),
    Range(Option<Date>, Option<Date>),
}

#[derive(Debug, Clone)]
struct Instance {
    id: String,
    processing_date: String,
}

async fn download_analytics(ctx: &Context, args: &Value, save_to: &str) -> ToolResult {
    let Some(app_id) = optional_str(args, "app_id")?.map(str::trim) else {
        return Err(ToolError::new(
            "kind=analytics needs app_id: the App Store Connect app ID (list_apps shows it)",
        ));
    };
    if app_id.is_empty() {
        return Err(ToolError::new("app_id must not be empty"));
    }
    let report_name = optional_str(args, "report_name")?
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let category = enum_arg(args, "category", ANALYTICS_CATEGORIES, "kind=analytics")?;
    if report_name.is_none() && category.is_none() {
        return Err(ToolError::new(
            "kind=analytics needs report_name (exact name, for example \"App Downloads \
             Standard\") or category (APP_USAGE, APP_STORE_ENGAGEMENT, COMMERCE, \
             FRAMEWORK_USAGE, PERFORMANCE)",
        ));
    }
    let granularity = enum_arg(
        args,
        "granularity",
        ANALYTICS_GRANULARITIES,
        "kind=analytics",
    )?
    .unwrap_or("DAILY");
    let access_type = enum_arg(
        args,
        "access_type",
        ANALYTICS_ACCESS_TYPES,
        "kind=analytics",
    )?;
    let filter = instance_filter(args)?;
    let target = Target::resolve(ctx, save_to)?;

    // 1. Запрос отчётов приложения.
    let request_path = render_path("/v1/apps/{id}/analyticsReportRequests", |_| {
        Some(app_id.to_string())
    })?;
    let requests = list_all(
        ctx,
        ApiRequest::new(HttpMethod::Get, request_path)
            .with_query(
                "fields[analyticsReportRequests]",
                "accessType,stoppedDueToInactivity",
            )
            .with_query("limit", PAGE_LIMIT),
    )
    .await
    .map_err(|err| {
        analytics_error(
            err,
            &format!("the analytics report requests of app {app_id}"),
        )
    })?;
    let Some((request_id, request_access, stopped)) = pick_request(&requests, access_type) else {
        let which = match access_type {
            Some(access) => format!("no {access} analytics report request"),
            None => "no analytics report request".to_string(),
        };
        return Err(ToolError::new(format!(
            "app {app_id} has {which}, so there is no analytics data to download. This tool \
             only reads and does not create requests. To start report generation, call \
             {CREATE_REQUEST_TOOL} (needs `write` access for the server's \
             profile) with body {{\"data\": {{\"type\": \"analyticsReportRequests\", \
             \"attributes\": {{\"accessType\": \"ONGOING\"}}, \"relationships\": {{\"app\": \
             {{\"data\": {{\"type\": \"apps\", \"id\": \"{app_id}\"}}}}}}}}}}, ask the user \
             first. Apple then needs time, usually 1-2 days, to generate the first report \
             instances; call report_download again after that."
        )));
    };

    // 2. Отчёт по имени или категории.
    let report = find_report(ctx, &request_id, report_name, category).await?;

    // 3. Экземпляры.
    let instances_path = render_path("/v1/analyticsReports/{id}/instances", |_| {
        Some(report.0.clone())
    })?;
    let mut instances_request = ApiRequest::new(HttpMethod::Get, instances_path)
        .with_query("filter[granularity]", granularity)
        .with_query(
            "fields[analyticsReportInstances]",
            "granularity,processingDate",
        )
        .with_query("limit", PAGE_LIMIT);
    if let InstanceFilter::Date(date) = &filter {
        instances_request =
            instances_request.with_query("filter[processingDate]", date.to_string());
    }
    let items = list_all(ctx, instances_request)
        .await
        .map_err(|err| analytics_error(err, &format!("the instances of report `{}`", report.1)))?;
    let all: Vec<Instance> = items
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            let processing_date = item
                .pointer("/attributes/processingDate")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(Instance {
                id,
                processing_date,
            })
        })
        .collect();
    let selected = select_instances(&all, &filter, &report.1, granularity, stopped)?;

    // 4. Сегменты каждого экземпляра.
    let mut downloads: Vec<(String, String)> = Vec::new(); // (processing_date, url)
    let mut empty_instances: Vec<String> = Vec::new();
    for instance in &selected {
        let path = render_path("/v1/analyticsReportInstances/{id}/segments", |_| {
            Some(instance.id.clone())
        })?;
        let segments = list_all(
            ctx,
            ApiRequest::new(HttpMethod::Get, path)
                .with_query(
                    "fields[analyticsReportSegments]",
                    "checksum,sizeInBytes,url",
                )
                .with_query("limit", PAGE_LIMIT),
        )
        .await
        .map_err(|err| {
            analytics_error(
                err,
                &format!("the segments of the {} instance", instance.processing_date),
            )
        })?;
        let urls: Vec<String> = segments
            .iter()
            .filter_map(|s| s.pointer("/attributes/url").and_then(Value::as_str))
            .filter(|url| !url.is_empty())
            .map(str::to_string)
            .collect();
        if urls.is_empty() {
            empty_instances.push(instance.processing_date.clone());
        }
        downloads.extend(
            urls.into_iter()
                .map(|url| (instance.processing_date.clone(), url)),
        );
    }
    if downloads.is_empty() {
        return Err(ToolError::new(format!(
            "the report `{}` instance(s) for {} are not ready yet: Apple has not published any \
             segments for them. Try again later (instances usually get their data within a day).",
            report.1,
            join_dates(&empty_instances)
        )));
    }

    // 5. Скачивание и склейка: по одному сегменту во временный файл.
    let total = downloads.len();
    let mut table: Option<TableWriter> = None;
    for (index, (date, url)) in downloads.iter().enumerate() {
        let temp = TempFile::new(&target, index + 1);
        ctx.external
            .download_to(url, &temp.path)
            .await
            .map_err(|err| {
                ToolError::new(scrub_url(
                    format!(
                        "could not download segment {} of {total} (instance {date}): {err}",
                        index + 1
                    ),
                    url,
                ))
            })?;
        let writer = match table.take() {
            Some(writer) => writer,
            None => TableWriter::create(&target)?,
        };
        let label = format!("segment {} of {total} (instance {date})", index + 1);
        table = Some(append_blocking(writer, temp.path.clone(), label).await?);
    }
    let Some(table) = table else {
        return Err(ToolError::new("no report segments were downloaded"));
    };
    let done = table.finish()?;

    let mut summary = done.stats.summary();
    let dates: BTreeSet<&str> = selected
        .iter()
        .map(|i| i.processing_date.as_str())
        .collect();
    if let (Some(first), Some(last)) = (dates.first(), dates.last()) {
        summary.insert(
            "processing_dates".into(),
            json!({"from": first, "to": last}),
        );
    }
    summary.insert("instances".into(), json!(selected.len()));
    summary.insert("segments".into(), json!(total));
    if !empty_instances.is_empty() {
        summary.insert(
            "instances_without_data".into(),
            json!(empty_instances.len()),
        );
    }
    let mut message = describe("analytics", &done, &summary);
    message.push_str(&format!(
        " Joined {total} segment(s) of {} instance(s).",
        selected.len()
    ));
    if stopped {
        message.push_str(
            " Note: the ONGOING report request was stopped by Apple due to inactivity, so newer \
             data is not generated.",
        );
    }
    summary.insert("message".into(), json!(message));

    Ok(json!({
        "path": done.path.display().to_string(),
        "kind": "analytics",
        "rows": done.rows,
        "columns": done.columns,
        "bytes": done.bytes,
        "report": {
            "app_id": app_id,
            "report_name": report.1,
            "category": report.2,
            "granularity": granularity,
            "access_type": request_access,
            "report_request_id": request_id,
            "report_id": report.0,
        },
        "summary": Value::Object(summary),
    }))
}

fn instance_filter(args: &Value) -> Result<InstanceFilter, ToolError> {
    let date = |name: &str| -> Result<Option<Date>, ToolError> {
        optional_str(args, name)?
            .map(|value| {
                Date::parse(value).ok_or_else(|| {
                    ToolError::new(format!("invalid {name} `{value}`: expected YYYY-MM-DD"))
                })
            })
            .transpose()
    };
    let processing = date("processing_date")?;
    let from = date("date_from")?;
    let to = date("date_to")?;
    match (processing, from, to) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(ToolError::new(
            "pass either processing_date or date_from/date_to, not both",
        )),
        (Some(date), None, None) => Ok(InstanceFilter::Date(date)),
        (None, None, None) => Ok(InstanceFilter::Latest),
        (None, from, to) => {
            if let (Some(from), Some(to)) = (&from, &to)
                && from > to
            {
                return Err(ToolError::new("date_from is after date_to"));
            }
            Ok(InstanceFilter::Range(from, to))
        }
    }
}

/// Запрос отчётов: заданного типа или `ONGOING` (сначала активный), затем
/// `ONE_TIME_SNAPSHOT`. Возвращает (id, accessType, остановлен ли).
fn pick_request(requests: &[Value], access_type: Option<&str>) -> Option<(String, String, bool)> {
    let parsed: Vec<(String, String, bool)> = requests
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            let access = item
                .pointer("/attributes/accessType")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let stopped = item
                .pointer("/attributes/stoppedDueToInactivity")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some((id, access, stopped))
        })
        .collect();
    let rank = |(_, access, stopped): &(String, String, bool)| -> Option<u8> {
        match (access_type, access.as_str(), stopped) {
            (Some(wanted), got, false) if wanted == got => Some(0),
            (Some(wanted), got, true) if wanted == got => Some(1),
            (Some(_), _, _) => None,
            (None, "ONGOING", false) => Some(0),
            (None, "ONE_TIME_SNAPSHOT", _) => Some(1),
            (None, "ONGOING", true) => Some(2),
            (None, _, _) => Some(3),
        }
    };
    parsed
        .into_iter()
        .filter_map(|item| rank(&item).map(|r| (r, item)))
        .min_by_key(|(r, _)| *r)
        .map(|(_, item)| item)
}

/// Отчёт запроса по имени или категории. Возвращает (id, name, category).
async fn find_report(
    ctx: &Context,
    request_id: &str,
    report_name: Option<&str>,
    category: Option<&str>,
) -> Result<(String, String, String), ToolError> {
    let path = render_path("/v1/analyticsReportRequests/{id}/reports", |_| {
        Some(request_id.to_string())
    })?;
    let base = ApiRequest::new(HttpMethod::Get, path)
        .with_query("fields[analyticsReports]", "name,category")
        .with_query("limit", PAGE_LIMIT);
    let with_category = |request: ApiRequest| match category {
        Some(category) => request.with_query("filter[category]", category),
        None => request,
    };
    let parse = |items: &[Value]| -> Vec<(String, String, String)> {
        items
            .iter()
            .filter_map(|item| {
                let id = item.get("id")?.as_str()?.to_string();
                let attr = |key: &str| {
                    item.pointer(&format!("/attributes/{key}"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                Some((id, attr("name"), attr("category")))
            })
            .collect()
    };
    let context = "the reports of the analytics report request";

    if let Some(name) = report_name {
        let request = with_category(base.clone()).with_query("filter[name]", name);
        let found = parse(
            &list_all(ctx, request)
                .await
                .map_err(|err| analytics_error(err, context))?,
        );
        let exact: Vec<_> = found
            .iter()
            .filter(|(_, n, _)| n.eq_ignore_ascii_case(name))
            .cloned()
            .collect();
        match (exact.len(), found.len()) {
            (1, _) => {
                return exact
                    .into_iter()
                    .next()
                    .ok_or_else(|| ToolError::new("no report"));
            }
            (0, 1) => {
                return found
                    .into_iter()
                    .next()
                    .ok_or_else(|| ToolError::new("no report"));
            }
            _ => {}
        }
        // Имени нет (или неоднозначно): подсказываем доступные имена.
        let available = parse(
            &list_all(ctx, with_category(base))
                .await
                .map_err(|err| analytics_error(err, context))?,
        );
        return Err(ToolError::new(format!(
            "no analytics report named `{name}`{} for this app. Available report names: {}. \
             Pass one of them as report_name (exact spelling).",
            category
                .map(|c| format!(" in category {c}"))
                .unwrap_or_default(),
            list_names(&available)
        )));
    }

    let found = parse(
        &list_all(ctx, with_category(base))
            .await
            .map_err(|err| analytics_error(err, context))?,
    );
    let category = category.unwrap_or_default();
    match found.len() {
        0 => Err(ToolError::new(format!(
            "the analytics report request has no reports in category {category} yet. Apple \
             adds reports after it processes a new request (usually 1-2 days); try again later \
             or choose another category."
        ))),
        1 => found
            .into_iter()
            .next()
            .ok_or_else(|| ToolError::new("no report")),
        _ => Err(ToolError::new(format!(
            "category {category} has {} reports; pass report_name with one of: {}",
            found.len(),
            list_names(&found)
        ))),
    }
}

fn list_names(reports: &[(String, String, String)]) -> String {
    let names: BTreeSet<&str> = reports.iter().map(|(_, n, _)| n.as_str()).collect();
    if names.is_empty() {
        return "none yet".to_string();
    }
    let mut listed: Vec<String> = names
        .iter()
        .take(MAX_LISTED_NAMES)
        .map(|n| format!("\"{n}\""))
        .collect();
    if names.len() > MAX_LISTED_NAMES {
        listed.push(format!("and {} more", names.len() - MAX_LISTED_NAMES));
    }
    listed.join(", ")
}

fn select_instances(
    all: &[Instance],
    filter: &InstanceFilter,
    report_name: &str,
    granularity: &str,
    stopped: bool,
) -> Result<Vec<Instance>, ToolError> {
    let mut sorted: Vec<Instance> = all.to_vec();
    sorted.sort_by(|a, b| a.processing_date.cmp(&b.processing_date));
    let available = || match (sorted.first(), sorted.last()) {
        (Some(first), Some(last)) => format!(
            "available {granularity} processing dates: {} to {} ({} instance(s))",
            first.processing_date,
            last.processing_date,
            sorted.len()
        ),
        _ => format!("there are no {granularity} instances yet"),
    };
    let today = Date::today();
    let selected: Vec<Instance> = match filter {
        InstanceFilter::Latest => {
            let Some(latest) = sorted.last().map(|i| i.processing_date.clone()) else {
                return Err(ToolError::new(format!(
                    "the report `{report_name}` has no {granularity} instances yet: Apple has not \
                     generated data for it. After a report request is created it usually takes \
                     1-2 days for the first instances to appear{}; try again later or choose \
                     another granularity.",
                    if stopped {
                        " (and this request was stopped due to inactivity)"
                    } else {
                        ""
                    }
                )));
            };
            sorted
                .iter()
                .filter(|i| i.processing_date == latest)
                .cloned()
                .collect()
        }
        InstanceFilter::Date(date) => {
            let wanted = date.to_string();
            let found: Vec<Instance> = sorted
                .iter()
                .filter(|i| i.processing_date == wanted)
                .cloned()
                .collect();
            if found.is_empty() {
                let reason = if *date >= today.add_days(-1) {
                    format!(
                        "the instance for {wanted} is not ready yet: Apple generates instances \
                         after the day is processed, usually within 1-2 days. Try again later or \
                         choose an earlier processing_date"
                    )
                } else {
                    format!(
                        "there is no {granularity} instance for {wanted}; {}",
                        available()
                    )
                };
                return Err(ToolError::new(format!("report `{report_name}`: {reason}.")));
            }
            found
        }
        InstanceFilter::Range(from, to) => {
            let from_s = from.map(|d| d.to_string());
            let to_s = to.map(|d| d.to_string());
            let found: Vec<Instance> = sorted
                .iter()
                .filter(|i| from_s.as_ref().is_none_or(|f| &i.processing_date >= f))
                .filter(|i| to_s.as_ref().is_none_or(|t| &i.processing_date <= t))
                .cloned()
                .collect();
            if found.is_empty() {
                let future = from.is_some_and(|f| f >= today.add_days(-1));
                return Err(ToolError::new(format!(
                    "report `{report_name}` has no {granularity} instances between {} and {}{}; {}.",
                    from_s.as_deref().unwrap_or("the first one"),
                    to_s.as_deref().unwrap_or("now"),
                    if future {
                        " (data for recent days is not ready yet: Apple needs 1-2 days)"
                    } else {
                        ""
                    },
                    available()
                )));
            }
            found
        }
    };
    if selected.len() > MAX_INSTANCES {
        return Err(ToolError::new(format!(
            "{} instances match; at most {MAX_INSTANCES} can be joined in one call. Narrow \
             date_from/date_to or use a coarser granularity.",
            selected.len()
        )));
    }
    Ok(selected)
}

fn join_dates(dates: &[String]) -> String {
    match dates {
        [] => "the selected dates".to_string(),
        [one] => one.clone(),
        [first, .., last] => format!("{first} to {last}"),
    }
}

/// Ошибка запроса к API аналитики.
fn analytics_error(err: ApiError, what: &str) -> ToolError {
    match &err {
        ApiError::Http(http) if http.status == 404 => ToolError::new(format!(
            "App Store Connect could not find {what} (Apple: {}). Check the app ID and that the \
             API key has access to this app.",
            apple_detail(http)
        )),
        ApiError::Http(http) if http.status == 403 => ToolError::new(format!(
            "App Store Connect denied access to {what} (Apple: {}). Analytics reports need an API \
             key with the Admin, Finance, Sales and Reports or similar role.",
            apple_detail(http)
        )),
        _ => ToolError::new(format!("could not read {what}: {err}")),
    }
}

/// Все записи `data` списка по страницам (`links.next`), не больше [`MAX_LIST_ITEMS`].
async fn list_all(ctx: &Context, request: ApiRequest) -> Result<Vec<Value>, ApiError> {
    let mut items = Vec::new();
    let mut response: ApiResponse = ctx.api.execute(request).await?;
    loop {
        if let ResponseBody::Json(body) = &response.body
            && let Some(data) = body.get("data").and_then(Value::as_array)
        {
            items.extend(data.iter().cloned());
        }
        if items.len() >= MAX_LIST_ITEMS {
            break;
        }
        match response.pagination.next.take() {
            Some(next) => response = ctx.api.execute_next(&next).await?,
            None => break,
        }
    }
    Ok(items)
}

/// Убирает подписанную ссылку (и её query с подписью) из текста ошибки.
fn scrub_url(mut text: String, url: &str) -> String {
    if !url.is_empty() {
        text = text.replace(url, "<signed URL>");
    }
    if let Some((_, query)) = url.split_once('?')
        && !query.is_empty()
    {
        text = text.replace(query, "***");
    }
    text
}

// ---------------------------------------------------------------------------------------
// Файлы: временные gzip и итоговая таблица
// ---------------------------------------------------------------------------------------

/// Итоговый файл: канонический каталог и имя.
struct Target {
    dir: PathBuf,
    name: String,
    path: PathBuf,
}

impl Target {
    fn resolve(ctx: &Context, save_to: &str) -> Result<Self, ToolError> {
        let resolved = ctx.files.resolve_write(save_to)?;
        let (Some(parent), Some(name)) = (resolved.parent(), resolved.file_name()) else {
            return Err(ToolError::new(format!(
                "save_to must point to a file: {save_to}"
            )));
        };
        // Исполнитель принимает save_to только в каноническом каталоге (без ссылок): каталог
        // уже проверен правилами файлов сервера, здесь лишь раскрываем его до реального пути.
        let dir = std::fs::canonicalize(parent).map_err(|e| {
            ToolError::new(format!(
                "the directory for save_to is not available: {}: {e}",
                parent.display()
            ))
        })?;
        let name = name.to_string_lossy().into_owned();
        let path = dir.join(&name);
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(ToolError::new(format!(
                "file already exists and is not overwritten: {save_to}; choose a new save_to"
            )));
        }
        Ok(Self { dir, name, path })
    }
}

/// Временный файл рядом с итоговым; удаляется при выходе из области видимости.
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(target: &Target, index: usize) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            ".{}.{}-{unique}-{index}.{TEMP_SUFFIX}",
            target.name,
            std::process::id()
        );
        Self {
            path: target.dir.join(name),
        }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Распаковка и дописывание источника в таблицу вне потоков рантайма.
async fn append_blocking(
    mut table: TableWriter,
    source: PathBuf,
    label: String,
) -> Result<TableWriter, ToolError> {
    tokio::task::spawn_blocking(move || {
        table.append(&source, &label)?;
        Ok(table)
    })
    .await
    .map_err(|e| ToolError::new(format!("report decompression failed: {e}")))?
}

/// Итоговая таблица: строки пишутся сразу в файл, заголовок — один раз.
struct TableWriter {
    out: Option<BufWriter<File>>,
    path: PathBuf,
    /// Файл создан нами и удаляется, если вызов не дошёл до конца.
    keep: bool,
    header: Option<Vec<u8>>,
    columns: Vec<String>,
    delimiter: u8,
    rows: u64,
    bytes: u64,
    stats: Stats,
}

/// Готовая таблица.
struct Finished {
    path: PathBuf,
    columns: Vec<String>,
    rows: u64,
    bytes: u64,
    stats: Stats,
}

impl TableWriter {
    fn create(target: &Target) -> Result<Self, ToolError> {
        // create_new: существующий файл не перезаписывается, симлинк не открывается.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target.path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    ToolError::new(format!(
                        "file already exists and is not overwritten: {}",
                        target.path.display()
                    ))
                } else {
                    ToolError::new(format!("could not create {}: {e}", target.path.display()))
                }
            })?;
        Ok(Self {
            out: Some(BufWriter::new(file)),
            path: target.path.clone(),
            keep: false,
            header: None,
            columns: Vec::new(),
            delimiter: b'\t',
            rows: 0,
            bytes: 0,
            stats: Stats::default(),
        })
    }

    /// Дописывает распакованный источник (gzip или обычный текст). Первая непустая строка
    /// источника — заголовок: у первого источника он пишется, у следующих должен совпасть
    /// и пропускается.
    fn append(&mut self, source: &Path, label: &str) -> Result<(), ToolError> {
        let io = |e: std::io::Error| ToolError::new(format!("could not read {label}: {e}"));
        let file = File::open(source).map_err(io)?;
        let mut raw = BufReader::new(file);
        let is_gzip = raw.fill_buf().map_err(io)?.starts_with(&[0x1f, 0x8b]);
        let mut reader: Box<dyn BufRead> = if is_gzip {
            Box::new(BufReader::new(MultiGzDecoder::new(raw)))
        } else {
            Box::new(raw)
        };
        let mut line = Vec::with_capacity(4096);
        let mut header_seen = false;
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line).map_err(|e| {
                ToolError::new(format!(
                    "could not decompress {label}: {e} (the file is not valid gzip)"
                ))
            })?;
            if read == 0 {
                break;
            }
            let mut content = trim_eol(&line);
            if !header_seen {
                content = content.strip_prefix(b"\xef\xbb\xbf").unwrap_or(content);
            }
            if content.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            if !header_seen {
                header_seen = true;
                match &self.header {
                    None => {
                        self.delimiter = if content.contains(&b'\t') {
                            b'\t'
                        } else {
                            b','
                        };
                        self.columns = split_fields(content, self.delimiter)
                            .into_iter()
                            .map(|c| c.trim().to_string())
                            .collect();
                        self.stats = Stats::for_columns(&self.columns);
                        self.header = Some(content.to_vec());
                        let header = content.to_vec();
                        self.write_line(&header)?;
                        continue;
                    }
                    Some(header) if header.as_slice() == content => continue,
                    Some(_) => {
                        return Err(ToolError::new(format!(
                            "{label} has different columns than the first segment; the segments \
                             cannot be joined into one table"
                        )));
                    }
                }
            }
            self.rows += 1;
            let fields = split_fields(content, self.delimiter);
            self.stats.observe(&fields, self.columns.len());
            let row = content.to_vec();
            self.write_line(&row)?;
        }
        Ok(())
    }

    fn write_line(&mut self, content: &[u8]) -> Result<(), ToolError> {
        let Some(out) = self.out.as_mut() else {
            return Err(ToolError::new("the report file is already closed"));
        };
        let io = |e: std::io::Error| ToolError::new(format!("could not write the report: {e}"));
        out.write_all(content).map_err(io)?;
        out.write_all(b"\n").map_err(io)?;
        self.bytes += content.len() as u64 + 1;
        if self.bytes > MAX_REPORT_BYTES {
            return Err(ToolError::new(format!(
                "the decompressed report is larger than {} GiB; download a shorter period",
                MAX_REPORT_BYTES / (1024 * 1024 * 1024)
            )));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Finished, ToolError> {
        if let Some(out) = self.out.take() {
            let file = out.into_inner().map_err(|e| {
                ToolError::new(format!("could not write the report: {}", e.error()))
            })?;
            file.sync_all()
                .map_err(|e| ToolError::new(format!("could not write the report: {e}")))?;
        }
        self.keep = true;
        Ok(Finished {
            path: self.path.clone(),
            columns: std::mem::take(&mut self.columns),
            rows: self.rows,
            bytes: self.bytes,
            stats: std::mem::take(&mut self.stats),
        })
    }
}

impl Drop for TableWriter {
    fn drop(&mut self) {
        if !self.keep {
            // Неполный отчёт вводит в заблуждение: удаляем файл, который создали сами.
            self.out.take();
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn trim_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Поля строки: TSV — по табуляции, CSV — по запятой с учётом кавычек.
fn split_fields(line: &[u8], delimiter: u8) -> Vec<String> {
    let text = String::from_utf8_lossy(line);
    if delimiter == b'\t' {
        return text.split('\t').map(str::to_string).collect();
    }
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    fields.push(current);
    fields
}

// ---------------------------------------------------------------------------------------
// Сводка
// ---------------------------------------------------------------------------------------

/// Колонки дат начала и конца периода строки (продажи, финансы, аналитика).
const START_DATE_COLUMNS: &[&str] = &["Begin Date", "Start Date", "Date"];
const END_DATE_COLUMNS: &[&str] = &["End Date", "Date"];
/// Колонки, которые суммируются.
const SUM_COLUMNS: &[&str] = &["Units", "Quantity", "Counts"];
/// Колонки, для которых считается число различных значений.
const DISTINCT_COLUMNS: &[&str] = &[
    "SKU",
    "Vendor Identifier",
    "Apple Identifier",
    "Country Code",
    "Country Of Sale",
    "Territory",
];

#[derive(Debug, Default)]
struct Stats {
    start: Option<usize>,
    end: Option<usize>,
    min_date: Option<String>,
    max_date: Option<String>,
    sums: Vec<(String, usize, f64, bool)>,
    distinct: Vec<(String, usize, BTreeSet<String>, bool)>,
    /// Строки, число полей которых не совпало с заголовком (итоги в конце финансовых
    /// отчётов и т. п.): в сводку не входят.
    other_rows: u64,
}

impl Stats {
    fn for_columns(columns: &[String]) -> Self {
        let index = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
        let first = |names: &[&str]| names.iter().find_map(|n| index(n));
        Self {
            start: first(START_DATE_COLUMNS),
            end: first(END_DATE_COLUMNS),
            sums: SUM_COLUMNS
                .iter()
                .filter_map(|n| index(n).map(|i| (columns[i].clone(), i, 0.0, false)))
                .collect(),
            distinct: DISTINCT_COLUMNS
                .iter()
                .filter_map(|n| index(n).map(|i| (columns[i].clone(), i, BTreeSet::new(), false)))
                .collect(),
            ..Self::default()
        }
    }

    fn observe(&mut self, fields: &[String], width: usize) {
        if fields.len() != width {
            self.other_rows += 1;
            return;
        }
        let field = |i: Option<usize>| i.and_then(|i| fields.get(i)).map(|f| f.trim());
        if let Some(date) = field(self.start).and_then(normalize_date)
            && self.min_date.as_ref().is_none_or(|min| &date < min)
        {
            self.min_date = Some(date);
        }
        if let Some(date) = field(self.end).and_then(normalize_date)
            && self.max_date.as_ref().is_none_or(|max| &date > max)
        {
            self.max_date = Some(date);
        }
        for (_, index, sum, seen) in &mut self.sums {
            if let Some(value) = fields
                .get(*index)
                .and_then(|v| v.trim().parse::<f64>().ok())
            {
                *sum += value;
                *seen = true;
            }
        }
        for (_, index, values, overflow) in &mut self.distinct {
            if let Some(value) = fields
                .get(*index)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
            {
                if values.len() < MAX_DISTINCT {
                    values.insert(value.to_string());
                } else if !values.contains(value) {
                    *overflow = true;
                }
            }
        }
    }

    fn summary(&self) -> Map<String, Value> {
        let mut out = Map::new();
        if let (Some(from), Some(to)) = (&self.min_date, &self.max_date) {
            out.insert("date_range".into(), json!({"from": from, "to": to}));
        }
        let mut totals = Map::new();
        for (name, _, sum, seen) in &self.sums {
            if *seen {
                totals.insert(name.clone(), number(*sum));
            }
        }
        if !totals.is_empty() {
            out.insert("totals".into(), Value::Object(totals));
        }
        let mut distinct = Map::new();
        for (name, _, values, overflow) in &self.distinct {
            if !values.is_empty() {
                let count = if *overflow {
                    json!(format!("{}+", values.len()))
                } else {
                    json!(values.len())
                };
                distinct.insert(name.clone(), count);
            }
        }
        if !distinct.is_empty() {
            out.insert("distinct".into(), Value::Object(distinct));
        }
        if self.other_rows > 0 {
            out.insert("rows_not_in_totals".into(), json!(self.other_rows));
        }
        out
    }
}

/// Целое — как целое, иначе с двумя знаками.
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        json!(value as i64)
    } else {
        json!((value * 100.0).round() / 100.0)
    }
}

/// Дата строки отчёта в `YYYY-MM-DD`: продажи пишут `MM/DD/YYYY`, аналитика — `YYYY-MM-DD`.
fn normalize_date(value: &str) -> Option<String> {
    if let Some(date) = Date::parse(value) {
        return Some(date.to_string());
    }
    let mut parts = value.split('/');
    let (m, d, y) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let date = Date::new(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)?;
    Some(date.to_string())
}

/// Короткая фраза для агента.
fn describe(kind: &str, done: &Finished, summary: &Map<String, Value>) -> String {
    let mut text = format!(
        "Saved the {kind} report: {} data row(s), {} column(s), {} byte(s).",
        done.rows,
        done.columns.len(),
        done.bytes
    );
    if let Some(range) = summary.get("date_range") {
        let from = range
            .get("from")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let to = range.get("to").and_then(Value::as_str).unwrap_or_default();
        if from == to {
            text.push_str(&format!(" Dates: {from}."));
        } else {
            text.push_str(&format!(" Dates: {from} to {to}."));
        }
    }
    if let Some(Value::Object(totals)) = summary.get("totals") {
        for (name, value) in totals {
            text.push_str(&format!(" Total {name}: {value}."));
        }
    }
    if let Some(Value::Object(distinct)) = summary.get("distinct") {
        let parts: Vec<String> = distinct
            .iter()
            .map(|(name, count)| format!("{count} {name}", count = plain(count)))
            .collect();
        text.push_str(&format!(" Distinct: {}.", parts.join(", ")));
    }
    text
}

fn plain(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// Даты и периоды
// ---------------------------------------------------------------------------------------

/// Календарная дата (григорианский календарь, UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Date {
    year: i32,
    month: u32,
    day: u32,
}

impl Date {
    fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        if !(1..=9999).contains(&year)
            || !(1..=12).contains(&month)
            || day == 0
            || day > days_in_month(year, month)
        {
            return None;
        }
        Some(Self { year, month, day })
    }

    /// `YYYY-MM-DD`.
    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let bytes = value.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return None;
        }
        Self::new(
            parse_digits(&value[0..4])? as i32,
            parse_digits(&value[5..7])?,
            parse_digits(&value[8..10])?,
        )
    }

    fn today() -> Self {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        Self::from_days((secs / 86_400) as i64)
    }

    /// Дата по числу дней от 1970-01-01 (алгоритм civil_from_days Г. Хиннанта).
    fn from_days(days: i64) -> Self {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        Self { year, month, day }
    }

    /// Число дней от 1970-01-01 (days_from_civil).
    fn to_days(self) -> i64 {
        let y = i64::from(self.year) - i64::from(self.month <= 2);
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let m = i64::from(self.month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    fn add_days(self, days: i64) -> Self {
        Self::from_days(self.to_days() + days)
    }
}

impl std::fmt::Display for Date {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

fn parse_digits(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// Период отчёта продаж или финансов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    Day(Date),
    Month(i32, u32),
    Year(i32),
}

impl Period {
    /// Дата отчёта в формате, который требует частота.
    fn parse(frequency: &str, value: &str) -> Result<Self, ToolError> {
        let value = value.trim();
        let invalid = |format: &str| {
            ToolError::new(format!(
                "invalid report_date `{value}` for frequency {frequency}: expected {format}"
            ))
        };
        match frequency {
            "DAILY" | "WEEKLY" => Date::parse(value)
                .map(Period::Day)
                .ok_or_else(|| invalid("YYYY-MM-DD")),
            "MONTHLY" => {
                let (year, month) = value.split_once('-').ok_or_else(|| invalid("YYYY-MM"))?;
                match (
                    year.len(),
                    parse_digits(year),
                    month.len(),
                    parse_digits(month),
                ) {
                    (4, Some(year), 2, Some(month)) if (1..=12).contains(&month) => {
                        Ok(Period::Month(year as i32, month))
                    }
                    _ => Err(invalid("YYYY-MM")),
                }
            }
            "YEARLY" => match (value.len(), parse_digits(value)) {
                (4, Some(year)) => Ok(Period::Year(year as i32)),
                _ => Err(invalid("YYYY")),
            },
            other => Err(ToolError::new(format!("unknown frequency `{other}`"))),
        }
    }

    /// Последний день периода.
    fn end(&self) -> Date {
        match *self {
            Period::Day(date) => date,
            Period::Month(year, month) => Date {
                year,
                month,
                day: days_in_month(year, month),
            },
            Period::Year(year) => Date {
                year,
                month: 12,
                day: 31,
            },
        }
    }
}

/// Частота по формату даты: `YYYY-MM-DD` — DAILY, `YYYY-MM` — MONTHLY, `YYYY` — YEARLY.
fn infer_frequency(date: &str) -> Result<&'static str, ToolError> {
    match date.trim().len() {
        10 => Ok("DAILY"),
        7 => Ok("MONTHLY"),
        4 => Ok("YEARLY"),
        _ => Err(ToolError::new(format!(
            "invalid report_date `{date}`: expected YYYY-MM-DD (daily or weekly), YYYY-MM \
             (monthly) or YYYY (yearly)"
        ))),
    }
}

/// Версия отчёта: `1_0`, `1_3` и т. п.
fn is_report_version(value: &str) -> bool {
    value.split_once('_').is_some_and(|(major, minor)| {
        parse_digits(major).is_some() && parse_digits(minor).is_some()
    })
}

// ---------------------------------------------------------------------------------------
// Аргументы
// ---------------------------------------------------------------------------------------

fn required_str<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    match optional_str(args, name)? {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(ToolError::new(format!(
            "missing required argument `{name}`"
        ))),
    }
}

fn optional_str<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(ToolError::new(format!(
            "argument `{name}` must be a string"
        ))),
    }
}

fn enum_arg(
    args: &Value,
    name: &str,
    allowed: &[&'static str],
    context: &str,
) -> Result<Option<&'static str>, ToolError> {
    let Some(value) = optional_str(args, name)? else {
        return Ok(None);
    };
    let value = value.trim();
    allowed
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(value))
        .copied()
        .map(Some)
        .ok_or_else(|| {
            ToolError::new(format!(
                "invalid {name} `{value}` for {context}: expected one of {}",
                allowed.join(", ")
            ))
        })
}
