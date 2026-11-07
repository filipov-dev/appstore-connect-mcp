//! `release_status`: сводка релиза приложения за один вызов.
//!
//! Четыре запроса чтения, все параллельно:
//!
//! 1. `GET /v1/apps/{id}` — имя, bundle ID, SKU;
//! 2. `GET /v1/apps/{id}/appStoreVersions` — версии платформы, кроме заменённых
//!    (`REPLACED_WITH_NEW_VERSION`), с выбранной сборкой и поэтапным выпуском (`include`);
//! 3. `GET /v1/reviewSubmissions` — отправки на проверку по приложению и платформе с
//!    элементами и версией (`include`);
//! 4. `GET /v1/builds` — последние сборки платформы с состоянием TestFlight
//!    (`buildBetaDetail`) и версией (`preReleaseVersion`).
//!
//! Отсутствие версии в подготовке, отправок или сборок — нормальное состояние: в ответе
//! `null` или пустой список, а не ошибка. Ошибка любого запроса — ошибка инструмента.

use std::collections::HashMap;

use appstore_connect_api::{
    AccessLevel, HttpMethod,
    executor::{ApiError, ApiRequest, ResponseBody, render_path},
};
use serde_json::{Value, json};

use super::{CompositeTool, Context, ToolError, ToolFuture};

/// Имя инструмента.
pub const NAME: &str = "release_status";

/// Платформы App Store Connect (`Platform` в спецификации).
const PLATFORMS: [&str; 4] = ["IOS", "MAC_OS", "TV_OS", "VISION_OS"];

const DEFAULT_PLATFORM: &str = "IOS";
const DEFAULT_BUILDS_LIMIT: u64 = 5;
const MAX_BUILDS_LIMIT: u64 = 50;

/// Сколько версий запрашивать. Заменённые версии отфильтрованы, поэтому у приложения
/// одновременно остаются единицы: опубликованная и одна в работе.
const VERSIONS_LIMIT: u32 = 20;

/// Сколько отправок на проверку запрашивать. Сортировки у `reviewSubmissions` нет, порядок
/// выдачи не документирован, поэтому берём максимум страницы и выбираем последнюю сами.
const SUBMISSIONS_LIMIT: u32 = 200;

/// Лимит тела каждого ответа. Ответы не уходят агенту целиком — из них собирается
/// короткая сводка, — поэтому лимит выше, чем для инструментов операций.
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;

/// Состояния версии, кроме `REPLACED_WITH_NEW_VERSION` (`AppVersionState` в спецификации).
const VERSION_STATES: &str = "ACCEPTED,DEVELOPER_REJECTED,IN_REVIEW,INVALID_BINARY,\
METADATA_REJECTED,PENDING_APPLE_RELEASE,PENDING_DEVELOPER_RELEASE,PREPARE_FOR_SUBMISSION,\
PROCESSING_FOR_DISTRIBUTION,READY_FOR_DISTRIBUTION,READY_FOR_REVIEW,REJECTED,\
WAITING_FOR_EXPORT_COMPLIANCE,WAITING_FOR_REVIEW";

/// Состояния отправки, при которых она ещё открыта (не `COMPLETE`).
const OPEN_SUBMISSION_STATES: [&str; 6] = [
    "READY_FOR_REVIEW",
    "WAITING_FOR_REVIEW",
    "IN_REVIEW",
    "UNRESOLVED_ISSUES",
    "CANCELING",
    "COMPLETING",
];

/// Доля пользователей по дням поэтапного выпуска (расписание Apple, 7 дней):
/// 1%, 2%, 5%, 10%, 20%, 50%, 100%.
const PHASED_PERCENT: [u64; 7] = [1, 2, 5, 10, 20, 50, 100];

/// Инструменты, на которые ссылаются подсказки `next_steps`. Тест проверяет, что они есть.
pub const REFERENCED_TOOLS: &[&str] = &[
    "create_app_store_version",
    "set_app_store_version_build",
    "update_build",
    "set_build_app_encryption_declaration",
    "create_review_submission",
    "create_review_submission_item",
    "update_review_submission",
    "create_app_store_version_release_request",
    "update_app_store_version_phased_release",
    "build_upload",
    "metadata_export",
];

const DESCRIPTION: &str = "Release overview of one app on one platform in a single call \
(4 read requests): the app (name, bundle ID, SKU), the live App Store version, the version \
in preparation or review with its selected build, release type and phased release, the \
latest App Review submission, the latest TestFlight builds with processing and beta states, \
and `next_steps` hints naming the tools to call next. Use it first when asked \"what is the \
status of the release\" or before preparing a submission. Missing pieces are normal: \
`pending_version` or `live_version` is null when there is none, `testflight_builds` is empty \
when nothing was uploaded. Read-only: it never changes anything.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: "versions-review",
        access: AccessLevel::Read,
        description: DESCRIPTION,
        input_schema,
        uses_local_files: false,
        run,
    }]
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "app_id": {
                "type": "string",
                "description": "App Store Connect app ID (the `id` from list_apps), not the bundle ID."
            },
            "platform": {
                "type": "string",
                "enum": PLATFORMS,
                "default": DEFAULT_PLATFORM,
                "description": "Platform of the versions and builds."
            },
            "builds_limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_BUILDS_LIMIT,
                "default": DEFAULT_BUILDS_LIMIT,
                "description": "How many latest TestFlight builds to show."
            }
        },
        "required": ["app_id"],
        "additionalProperties": false
    })
}

fn run<'a>(ctx: &'a Context, args: Value) -> ToolFuture<'a> {
    Box::pin(release_status(ctx, args))
}

/// Аргументы после проверки.
struct Args {
    app_id: String,
    platform: &'static str,
    builds_limit: u64,
}

/// Сервер уже проверил аргументы по схеме; здесь — защита в глубину и значения по умолчанию.
fn parse_args(args: &Value) -> Result<Args, ToolError> {
    let app_id = args
        .get("app_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ToolError::new("`app_id` is required: the App Store Connect app ID"))?;
    let platform = match args.get("platform") {
        None | Some(Value::Null) => DEFAULT_PLATFORM,
        Some(value) => {
            let name = value.as_str().unwrap_or_default();
            PLATFORMS
                .iter()
                .copied()
                .find(|p| *p == name)
                .ok_or_else(|| {
                    ToolError::new(format!(
                        "`platform` must be one of {}",
                        PLATFORMS.join(", ")
                    ))
                })?
        }
    };
    let builds_limit = match args.get("builds_limit") {
        None | Some(Value::Null) => DEFAULT_BUILDS_LIMIT,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=MAX_BUILDS_LIMIT).contains(n))
            .ok_or_else(|| {
                ToolError::new(format!(
                    "`builds_limit` must be an integer from 1 to {MAX_BUILDS_LIMIT}"
                ))
            })?,
    };
    Ok(Args {
        app_id: app_id.to_string(),
        platform,
        builds_limit,
    })
}

async fn release_status(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let args = parse_args(&args)?;
    let app_path = render_path("/v1/apps/{id}", |_| Some(args.app_id.clone()))?;
    let versions_path = render_path("/v1/apps/{id}/appStoreVersions", |_| {
        Some(args.app_id.clone())
    })?;

    let app_req =
        ApiRequest::new(HttpMethod::Get, app_path).with_query("fields[apps]", "name,bundleId,sku");
    let versions_req = ApiRequest::new(HttpMethod::Get, versions_path)
        .with_query("filter[platform]", args.platform)
        .with_query("filter[appVersionState]", VERSION_STATES)
        .with_query(
            "fields[appStoreVersions]",
            "platform,versionString,appStoreState,appVersionState,releaseType,\
             earliestReleaseDate,createdDate,build,appStoreVersionPhasedRelease",
        )
        .with_query(
            "fields[builds]",
            "version,uploadedDate,expired,processingState,usesNonExemptEncryption",
        )
        .with_query(
            "fields[appStoreVersionPhasedReleases]",
            "phasedReleaseState,startDate,totalPauseDuration,currentDayNumber",
        )
        .with_query("include", "build,appStoreVersionPhasedRelease")
        .with_query("limit", VERSIONS_LIMIT.to_string());
    let submissions_req = ApiRequest::new(HttpMethod::Get, "/v1/reviewSubmissions")
        .with_query("filter[app]", args.app_id.clone())
        .with_query("filter[platform]", args.platform)
        .with_query(
            "fields[reviewSubmissions]",
            "platform,submittedDate,state,items,appStoreVersionForReview",
        )
        .with_query("fields[reviewSubmissionItems]", "state")
        .with_query("fields[appStoreVersions]", "versionString")
        .with_query("include", "items,appStoreVersionForReview")
        .with_query("limit[items]", "50")
        .with_query("limit", SUBMISSIONS_LIMIT.to_string());
    let builds_req = ApiRequest::new(HttpMethod::Get, "/v1/builds")
        .with_query("filter[app]", args.app_id.clone())
        .with_query("filter[preReleaseVersion.platform]", args.platform)
        .with_query("sort", "-uploadedDate")
        .with_query(
            "fields[builds]",
            "version,uploadedDate,expirationDate,expired,processingState,\
             usesNonExemptEncryption,buildAudienceType,preReleaseVersion,buildBetaDetail",
        )
        .with_query(
            "fields[buildBetaDetails]",
            "internalBuildState,externalBuildState",
        )
        .with_query("fields[preReleaseVersions]", "version,platform")
        .with_query("include", "preReleaseVersion,buildBetaDetail")
        .with_query("limit", args.builds_limit.to_string());

    let (app, versions, submissions, builds) = tokio::join!(
        fetch(ctx, app_req, "the app"),
        fetch(ctx, versions_req, "App Store versions"),
        fetch(ctx, submissions_req, "review submissions"),
        fetch(ctx, builds_req, "builds"),
    );
    let (app, versions, submissions, builds) = (app?, versions?, submissions?, builds?);

    let app = summarize_app(&app);
    let (live, pending) = pick_versions(&versions);
    let review = summarize_review(&submissions);
    let builds = summarize_builds(&builds);
    let next_steps = next_steps(pending.as_ref(), live.as_ref(), review.as_ref(), &builds);

    Ok(json!({
        "app": app,
        "platform": args.platform,
        "live_version": live,
        "pending_version": pending,
        "review": review,
        "testflight_builds": builds,
        "next_steps": next_steps,
    }))
}

/// Один запрос: JSON-тело ответа или ошибка с указанием, что не удалось получить.
async fn fetch(ctx: &Context, request: ApiRequest, what: &str) -> Result<Value, ToolError> {
    let response = ctx
        .api
        .execute_with_limit(request, RESPONSE_LIMIT)
        .await
        .map_err(|err| describe(what, &err))?;
    match response.body {
        ResponseBody::Json(value) => Ok(value),
        _ => Err(ToolError::new(format!(
            "could not read {what}: the App Store Connect API returned no JSON"
        ))),
    }
}

fn describe(what: &str, err: &ApiError) -> ToolError {
    ToolError::new(format!("could not read {what}: {err}"))
}

/// Ресурсы `included` по паре (тип, id).
fn included(body: &Value) -> HashMap<(&str, &str), &Value> {
    body.get("included")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let kind = item.get("type")?.as_str()?;
            let id = item.get("id")?.as_str()?;
            Some(((kind, id), item))
        })
        .collect()
}

/// Связанный ресурс «к одному» из `included` (или только его id, если его нет в `included`).
fn related<'v>(
    resource: &'v Value,
    relationship: &str,
    included: &HashMap<(&str, &str), &'v Value>,
) -> Option<Related<'v>> {
    let data = resource
        .pointer(&format!("/relationships/{relationship}/data"))
        .filter(|d| d.is_object())?;
    let kind = data.get("type")?.as_str()?;
    let id = data.get("id")?.as_str()?;
    Some(Related {
        id,
        resource: included.get(&(kind, id)).copied(),
    })
}

struct Related<'v> {
    id: &'v str,
    resource: Option<&'v Value>,
}

fn attr<'v>(resource: &'v Value, name: &str) -> &'v Value {
    resource
        .pointer(&format!("/attributes/{name}"))
        .unwrap_or(&Value::Null)
}

fn attr_str<'v>(resource: &'v Value, name: &str) -> Option<&'v str> {
    attr(resource, name).as_str()
}

fn data_list(body: &Value) -> &[Value] {
    body.get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn summarize_app(body: &Value) -> Value {
    let data = body.get("data").unwrap_or(&Value::Null);
    json!({
        "id": data.get("id"),
        "name": attr(data, "name"),
        "bundle_id": attr(data, "bundleId"),
        "sku": attr(data, "sku"),
    })
}

/// Опубликованная версия и версия в работе.
///
/// Опубликованная — `appVersionState = READY_FOR_DISTRIBUTION` (у старых ответов без
/// `appVersionState` — устаревший `appStoreState = READY_FOR_SALE`). В работе — любая
/// другая незаменённая версия. Если таких несколько, берётся созданная последней.
fn pick_versions(body: &Value) -> (Option<Value>, Option<Value>) {
    let included = included(body);
    let mut live: Option<&Value> = None;
    let mut pending: Option<&Value> = None;
    for version in data_list(body) {
        let slot = if is_live(version) {
            &mut live
        } else {
            &mut pending
        };
        let newer = slot.is_none_or(|current| {
            attr_str(version, "createdDate").unwrap_or_default()
                > attr_str(current, "createdDate").unwrap_or_default()
        });
        if newer {
            *slot = Some(version);
        }
    }
    (
        live.map(|v| summarize_version(v, &included)),
        pending.map(|v| summarize_version(v, &included)),
    )
}

fn is_live(version: &Value) -> bool {
    match attr_str(version, "appVersionState") {
        Some(state) => state == "READY_FOR_DISTRIBUTION",
        None => attr_str(version, "appStoreState") == Some("READY_FOR_SALE"),
    }
}

fn summarize_version(version: &Value, included: &HashMap<(&str, &str), &Value>) -> Value {
    let build = related(version, "build", included).map(|b| {
        let r = b.resource.unwrap_or(&Value::Null);
        json!({
            "id": b.id,
            "version": attr(r, "version"),
            "processing_state": attr(r, "processingState"),
            "uploaded_date": attr(r, "uploadedDate"),
            "expired": attr(r, "expired"),
            "uses_non_exempt_encryption": attr(r, "usesNonExemptEncryption"),
        })
    });
    let phased = related(version, "appStoreVersionPhasedRelease", included).map(|p| {
        let r = p.resource.unwrap_or(&Value::Null);
        let state = attr_str(r, "phasedReleaseState");
        let day = attr(r, "currentDayNumber").as_u64();
        json!({
            "id": p.id,
            "state": state,
            "start_date": attr(r, "startDate"),
            "current_day_number": day,
            "total_pause_duration": attr(r, "totalPauseDuration"),
            "percent": phased_percent(state, day),
        })
    });
    json!({
        "id": version.get("id"),
        "version_string": attr(version, "versionString"),
        "app_version_state": attr(version, "appVersionState"),
        "app_store_state": attr(version, "appStoreState"),
        "release_type": attr(version, "releaseType"),
        "earliest_release_date": attr(version, "earliestReleaseDate"),
        "created_date": attr(version, "createdDate"),
        "build": build,
        "phased_release": phased,
    })
}

/// Доля пользователей, которым доступна версия при поэтапном выпуске.
fn phased_percent(state: Option<&str>, day: Option<u64>) -> Option<u64> {
    match state? {
        "COMPLETE" => Some(100),
        "INACTIVE" => Some(0),
        "ACTIVE" | "PAUSED" => {
            let day = day.filter(|d| *d >= 1)?;
            let index = usize::try_from(day - 1).unwrap_or(usize::MAX);
            Some(PHASED_PERCENT.get(index).copied().unwrap_or(100))
        }
        _ => None,
    }
}

/// Последняя отправка на проверку.
///
/// Открытая отправка (не `COMPLETE`) важнее завершённой: у платформы бывает только одна
/// открытая, и она мешает создать новую. Среди равных — с самой поздней `submittedDate`;
/// отправка без даты (черновик `READY_FOR_REVIEW`) считается самой новой.
fn summarize_review(body: &Value) -> Option<Value> {
    let included = included(body);
    let submissions = data_list(body);
    let rank = |s: &Value| {
        let open = attr_str(s, "state").is_some_and(|st| OPEN_SUBMISSION_STATES.contains(&st));
        let date = attr_str(s, "submittedDate");
        (open, date.is_none(), date.unwrap_or_default().to_string())
    };
    let latest = submissions.iter().max_by_key(|s| rank(s))?;
    let items = latest.pointer("/relationships/items");
    let items_count = items
        .and_then(|i| i.pointer("/meta/paging/total"))
        .and_then(Value::as_u64)
        .or_else(|| {
            items
                .and_then(|i| i.get("data"))
                .and_then(Value::as_array)
                .map(|d| d.len() as u64)
        });
    let version = related(latest, "appStoreVersionForReview", &included);
    let open_count = submissions
        .iter()
        .filter(|s| attr_str(s, "state").is_some_and(|st| OPEN_SUBMISSION_STATES.contains(&st)))
        .count();
    Some(json!({
        "id": latest.get("id"),
        "state": attr(latest, "state"),
        "submitted_date": attr(latest, "submittedDate"),
        "items_count": items_count,
        "app_store_version_id": version.as_ref().map(|v| v.id),
        "app_store_version_string": version
            .and_then(|v| v.resource)
            .map(|r| attr(r, "versionString").clone()),
        "open_submissions": open_count,
    }))
}

fn summarize_builds(body: &Value) -> Vec<Value> {
    let included = included(body);
    data_list(body)
        .iter()
        .map(|build| {
            let beta = related(build, "buildBetaDetail", &included).and_then(|b| b.resource);
            let pre_release =
                related(build, "preReleaseVersion", &included).and_then(|p| p.resource);
            json!({
                "id": build.get("id"),
                "version": attr(build, "version"),
                "app_version": pre_release.map(|p| attr(p, "version").clone()),
                "uploaded_date": attr(build, "uploadedDate"),
                "expiration_date": attr(build, "expirationDate"),
                "expired": attr(build, "expired"),
                "processing_state": attr(build, "processingState"),
                "uses_non_exempt_encryption": attr(build, "usesNonExemptEncryption"),
                "audience": attr(build, "buildAudienceType"),
                "internal_state": beta.map(|b| attr(b, "internalBuildState").clone()),
                "external_state": beta.map(|b| attr(b, "externalBuildState").clone()),
            })
        })
        .collect()
}

fn field<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value.get(key).and_then(Value::as_str)
}

/// Подсказки агенту: что делать дальше и каким инструментом. Ссылаться только на
/// инструменты из [`REFERENCED_TOOLS`].
fn next_steps(
    pending: Option<&Value>,
    live: Option<&Value>,
    review: Option<&Value>,
    builds: &[Value],
) -> Vec<String> {
    let mut steps = Vec::new();
    let destructive = "destructive-level: needs --allow-destructive and the user's explicit \
                       confirmation";

    if builds.is_empty() {
        steps.push(
            "No builds for this platform yet: upload one (build_upload, or Xcode / Transporter) \
             and wait until its processing_state is VALID."
                .to_string(),
        );
    } else if builds
        .iter()
        .any(|b| field(b, "processing_state") == Some("PROCESSING"))
    {
        steps.push(
            "A build is still processing: wait and call release_status again before selecting it."
                .to_string(),
        );
    }
    if builds.iter().any(|b| {
        field(b, "internal_state") == Some("MISSING_EXPORT_COMPLIANCE")
            || field(b, "external_state") == Some("MISSING_EXPORT_COMPLIANCE")
    }) {
        steps.push(
            "A build is missing export compliance: answer it with update_build \
             (usesNonExemptEncryption) or attach a declaration with \
             set_build_app_encryption_declaration (update_build is destructive-level)."
                .to_string(),
        );
    }

    let Some(pending) = pending else {
        let what = if live.is_some() {
            "No version is in preparation"
        } else {
            "The app has no App Store version for this platform"
        };
        steps.push(format!(
            "{what}: create one with create_app_store_version when you are ready to release."
        ));
        return steps;
    };

    let version = field(pending, "version_string").unwrap_or("the pending version");
    let state = field(pending, "app_version_state").unwrap_or_default();
    let build = pending.get("build").filter(|b| b.is_object());
    let editable = matches!(
        state,
        "PREPARE_FOR_SUBMISSION" | "DEVELOPER_REJECTED" | "REJECTED" | "METADATA_REJECTED"
    );

    match build {
        None if editable => steps.push(format!(
            "Version {version} has no build selected: pick a VALID build from \
             testflight_builds and attach it with set_app_store_version_build."
        )),
        Some(b) if field(b, "processing_state").is_some_and(|s| s != "VALID") => {
            steps.push(format!(
                "The build selected for version {version} is not VALID yet (processing_state \
                 {}): wait or select another build with set_app_store_version_build.",
                field(b, "processing_state").unwrap_or_default()
            ));
        }
        // Сборка без атрибутов (её нет в `included`) — не повод для подсказки.
        Some(b)
            if field(b, "processing_state").is_some()
                && b.get("uses_non_exempt_encryption")
                    .is_some_and(Value::is_null) =>
        {
            steps.push(format!(
                "Export compliance of the build selected for version {version} is not answered: \
                 use update_build (usesNonExemptEncryption) or \
                 set_build_app_encryption_declaration."
            ));
        }
        _ => {}
    }

    let review_state = review.and_then(|r| field(r, "state"));
    match state {
        "PREPARE_FOR_SUBMISSION" | "DEVELOPER_REJECTED" | "REJECTED" | "METADATA_REJECTED" => {
            if matches!(state, "REJECTED" | "METADATA_REJECTED") {
                steps.push(format!(
                    "Version {version} was rejected by App Review: read the messages in App \
                     Store Connect (the Resolution Center is not available in the API), fix \
                     the issues, then resubmit."
                ));
            }
            if review_state == Some("READY_FOR_REVIEW") {
                steps.push(format!(
                    "A draft review submission already exists: add version {version} to it \
                     with create_review_submission_item (if it is not there yet) and submit it \
                     with update_review_submission (submitted: true) — {destructive}."
                ));
            } else if review_state == Some("UNRESOLVED_ISSUES") {
                steps.push(format!(
                    "The review submission has unresolved issues: after fixing them, resubmit \
                     with update_review_submission (submitted: true) — {destructive}."
                ));
            } else {
                steps.push(format!(
                    "When metadata (metadata_export), screenshots and the build are ready, \
                     submit version {version}: create_review_submission, \
                     create_review_submission_item, then update_review_submission \
                     (submitted: true) — {destructive}."
                ));
            }
        }
        "WAITING_FOR_EXPORT_COMPLIANCE" => steps.push(format!(
            "Version {version} waits for export compliance: answer it for the selected build \
             with update_build (usesNonExemptEncryption) or set_build_app_encryption_declaration."
        )),
        "READY_FOR_REVIEW" | "WAITING_FOR_REVIEW" | "IN_REVIEW" => steps.push(format!(
            "Version {version} is with App Review ({state}): nothing to do, check again later."
        )),
        "PENDING_DEVELOPER_RELEASE" => steps.push(format!(
            "Version {version} is approved and waits for a manual release: release it with \
             create_app_store_version_release_request — {destructive}."
        )),
        "PENDING_APPLE_RELEASE" | "ACCEPTED" | "PROCESSING_FOR_DISTRIBUTION" => {
            steps.push(format!(
                "Version {version} is approved and will be released by Apple \
                 (release_type {}).",
                field(pending, "release_type").unwrap_or("unknown")
            ));
        }
        "INVALID_BINARY" => steps.push(format!(
            "The build of version {version} is invalid: upload a new build and select it with \
             set_app_store_version_build."
        )),
        _ => {}
    }

    for v in [live, Some(pending)].into_iter().flatten() {
        let Some(phased) = v.get("phased_release").filter(|p| p.is_object()) else {
            continue;
        };
        let version = field(v, "version_string").unwrap_or("the version");
        match field(phased, "state") {
            Some("ACTIVE") => steps.push(format!(
                "Phased release of version {version} is active (day {}, {}% of users): pause or \
                 complete it with update_app_store_version_phased_release if needed — \
                 {destructive}.",
                display(phased.get("current_day_number")),
                display(phased.get("percent")),
            )),
            Some("PAUSED") => steps.push(format!(
                "Phased release of version {version} is paused: resume it with \
                 update_app_store_version_phased_release (phasedReleaseState ACTIVE) — \
                 {destructive}."
            )),
            _ => {}
        }
    }
    steps
}

fn display(value: Option<&Value>) -> String {
    match value {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => "?".to_string(),
    }
}
