//! Составные инструменты `metadata_export` и `metadata_apply`: локализации версии
//! App Store (`appStoreVersionLocalizations`) и информации о приложении
//! (`appInfoLocalizations`) в локальных файлах.
//!
//! Формат каталога:
//!
//! - `<locale>.json` на каждую локаль (`en-US.json`, `zh-Hans.json`): объект с разделами
//!   `version` (поля локализации версии) и `appInfo` (поля локализации информации о
//!   приложении). Порядок ключей фиксированный, как в спецификации Apple. Значение поля —
//!   строка или `null` (поле пустое). JSON выбран вместо TOML: он без потерь передаёт
//!   `null` и переводы строк, а агенту и скриптам его проще править.
//! - `_meta.json`: приложение, версия, appInfo и время экспорта.
//!
//! Правила `metadata_apply`:
//!
//! - раздела или поля нет в файле — значение в App Store Connect не трогаем;
//! - `null` или пустая строка — поле очищается (`null` в `PATCH`);
//! - локали нет в App Store Connect — создаём локализацию (`POST`);
//! - у локали в App Store Connect нет файла — она «отсутствует локально» и удаляется
//!   только с `delete_missing_locales` и уровнем доступа `Destructive`;
//! - пустая и отсутствующая строка равны: App Store Connect возвращает их по-разному.
//!
//! Длины полей спецификация не описывает; лимиты взяты из документации App Store Connect
//! и считаются в символах Unicode (`chars`), как это делает App Store Connect.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use appstore_connect_api::{
    AccessLevel, HttpMethod,
    executor::{ApiRequest, ApiResponse, ResponseBody},
};
use serde_json::{Map, Value, json};

use super::{CompositeTool, Context, ToolError, ToolFuture, ToolResult};

/// Имя файла с описанием экспорта.
const META_FILE: &str = "_meta.json";

/// Версия формата каталога.
const FORMAT_VERSION: u64 = 1;

/// Длина превью текста в ответе.
const PREVIEW_CHARS: usize = 80;

/// Защита от бесконечной выдачи: больше страниц не читаем.
const MAX_PAGES: usize = 100;

/// Состояния версии (`appVersionState`, для старых ответов — `appStoreState`), в которых
/// метаданные версии можно менять. `WAITING_FOR_REVIEW` и `IN_REVIEW` сюда не входят:
/// App Store Connect запрещает правки, пока версию не отзовут с проверки.
const EDITABLE_VERSION_STATES: &[&str] = &[
    "PREPARE_FOR_SUBMISSION",
    "DEVELOPER_REJECTED",
    "REJECTED",
    "METADATA_REJECTED",
    "INVALID_BINARY",
    "READY_FOR_REVIEW",
];

/// Состояния appInfo (`state`, для старых ответов — `appStoreState`), в которых
/// локализации appInfo можно менять. Опубликованная информация (`READY_FOR_DISTRIBUTION`)
/// и заменённая (`REPLACED_WITH_NEW_INFO`) не редактируются.
const EDITABLE_APP_INFO_STATES: &[&str] = &[
    "PREPARE_FOR_SUBMISSION",
    "DEVELOPER_REJECTED",
    "REJECTED",
    "METADATA_REJECTED",
    "READY_FOR_REVIEW",
];

const PLATFORMS: &[&str] = &["IOS", "MAC_OS", "TV_OS", "VISION_OS"];

pub fn tools() -> Vec<CompositeTool> {
    vec![
        CompositeTool {
            name: "metadata_export",
            toolset: "metadata",
            access: AccessLevel::Read,
            description: EXPORT_DESCRIPTION,
            input_schema: export_schema,
            uses_local_files: true,
            run: run_export,
        },
        CompositeTool {
            name: "metadata_apply",
            toolset: "metadata",
            access: AccessLevel::Write,
            description: APPLY_DESCRIPTION,
            input_schema: apply_schema,
            uses_local_files: true,
            run: run_apply,
        },
    ]
}

const EXPORT_DESCRIPTION: &str = "Export the App Store metadata of an app version to a local \
directory: one `<locale>.json` file per locale with the version localization fields \
(`version`: description, keywords, marketingUrl, promotionalText, supportUrl, whatsNew) and \
the app info localization fields (`appInfo`: name, subtitle, privacyPolicyUrl, \
privacyChoicesUrl, privacyPolicyText), plus `_meta.json` with the app, version and app info \
IDs. Without `version_id` the editable App Store version of `platform` is used (states \
PREPARE_FOR_SUBMISSION, DEVELOPER_REJECTED, REJECTED, METADATA_REJECTED, INVALID_BINARY, \
READY_FOR_REVIEW); without `app_info_id` the editable app info is used. The directory must \
not contain exported files yet. Edit the files, then call `metadata_apply` with the same \
directory (first with the default `dry_run: true`). The response lists the locales and files, \
not the texts: read the files to see them.";

const APPLY_DESCRIPTION: &str = "Apply a metadata directory written by `metadata_export` (and \
edited) to App Store Connect. Compares every `<locale>.json` with the current localizations \
and lists what would change: localizations to create, fields to change (old and new value \
previews), locales that exist in App Store Connect but have no file. With the default \
`dry_run: true` nothing is sent; review the diff and call again with `dry_run: false` to \
create new localizations and update only the changed fields. A field or section left out of a \
file is not touched; `null` or an empty string clears the field. Locales without a file are \
deleted only with `delete_missing_locales: true`, which needs a server with `destructive` \
access. Field lengths (name and subtitle 30, keywords 100, promotionalText \
170, description and whatsNew 4000 characters) and URLs are checked first; with any problem \
nothing is sent. The target version and app info come from `_meta.json`; pass `version_id` \
or `app_info_id` to apply the files to another (or a non-editable) version or app info. \
Applying the same directory twice sends no write requests the second time.";

fn export_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["app_id", "dir"],
        "properties": {
            "app_id": {
                "type": "string",
                "description": "App ID (the `id` of the app in App Store Connect)."
            },
            "dir": {
                "type": "string",
                "description": "Directory for the files. Created if missing; must not contain locale files or `_meta.json` yet."
            },
            "platform": {
                "type": "string",
                "enum": PLATFORMS,
                "default": "IOS",
                "description": "Platform of the App Store version to pick when `version_id` is not given."
            },
            "version_id": {
                "type": "string",
                "description": "App Store version ID. Default: the editable version of `platform`."
            },
            "app_info_id": {
                "type": "string",
                "description": "App info ID. Default: the editable app info of the app."
            }
        }
    })
}

fn apply_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["dir"],
        "properties": {
            "dir": {
                "type": "string",
                "description": "Directory written by `metadata_export`: `_meta.json` and `<locale>.json` files."
            },
            "dry_run": {
                "type": "boolean",
                "default": true,
                "description": "Only compute and return the diff (default). Set to false to apply it."
            },
            "delete_missing_locales": {
                "type": "boolean",
                "default": false,
                "description": "Delete localizations of locales that have no file. Needs the destructive access level."
            },
            "version_id": {
                "type": "string",
                "description": "Apply to this App Store version instead of the one in `_meta.json` (also skips the editable-state check)."
            },
            "app_info_id": {
                "type": "string",
                "description": "Apply to this app info instead of the one in `_meta.json` (also skips the editable-state check)."
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Поля и ресурсы
// ---------------------------------------------------------------------------

/// Поле локализации: имя атрибута, лимит длины, URL ли это.
struct FieldDef {
    name: &'static str,
    max_chars: Option<usize>,
    url: bool,
}

const fn text(name: &'static str, max: usize) -> FieldDef {
    FieldDef {
        name,
        max_chars: Some(max),
        url: false,
    }
}

const fn url(name: &'static str) -> FieldDef {
    FieldDef {
        name,
        max_chars: None,
        url: true,
    }
}

/// Записываемые атрибуты `AppStoreVersionLocalizationUpdateRequest` в порядке спецификации.
const VERSION_FIELDS: &[FieldDef] = &[
    text("description", 4000),
    text("keywords", 100),
    url("marketingUrl"),
    text("promotionalText", 170),
    url("supportUrl"),
    text("whatsNew", 4000),
];

/// Записываемые атрибуты `AppInfoLocalizationUpdateRequest` в порядке спецификации.
/// Лимита для `privacyPolicyText` в документации нет.
const INFO_FIELDS: &[FieldDef] = &[
    text("name", 30),
    text("subtitle", 30),
    url("privacyPolicyUrl"),
    url("privacyChoicesUrl"),
    FieldDef {
        name: "privacyPolicyText",
        max_chars: None,
        url: false,
    },
];

/// Вид локализации.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Version,
    Info,
}

const KINDS: [Kind; 2] = [Kind::Version, Kind::Info];

impl Kind {
    /// Раздел в файле локали.
    fn section(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Info => "appInfo",
        }
    }

    /// Тип ресурса JSON:API.
    fn resource(self) -> &'static str {
        match self {
            Self::Version => "appStoreVersionLocalizations",
            Self::Info => "appInfoLocalizations",
        }
    }

    fn fields(self) -> &'static [FieldDef] {
        match self {
            Self::Version => VERSION_FIELDS,
            Self::Info => INFO_FIELDS,
        }
    }

    fn field(self, name: &str) -> Option<&'static FieldDef> {
        self.fields().iter().find(|f| f.name == name)
    }

    /// Связь с родителем в запросе создания: имя и тип ресурса.
    fn parent(self) -> (&'static str, &'static str) {
        match self {
            Self::Version => ("appStoreVersion", "appStoreVersions"),
            Self::Info => ("appInfo", "appInfos"),
        }
    }

    fn list_path(self, parent_id: &str) -> String {
        match self {
            Self::Version => {
                format!("/v1/appStoreVersions/{parent_id}/appStoreVersionLocalizations")
            }
            Self::Info => format!("/v1/appInfos/{parent_id}/appInfoLocalizations"),
        }
    }

    fn collection_path(self) -> String {
        format!("/v1/{}", self.resource())
    }

    fn item_path(self, id: &str) -> String {
        format!("/v1/{}/{id}", self.resource())
    }
}

/// Значения полей одного раздела: имя поля → значение (`None` — пусто).
type Values = BTreeMap<&'static str, Option<String>>;

/// Локализация в App Store Connect.
#[derive(Debug, Clone)]
struct RemoteLoc {
    id: String,
    values: Values,
}

/// Локализации одного вида по локалям.
type RemoteMap = BTreeMap<String, RemoteLoc>;

/// Пустая строка и отсутствие значения для сравнения одинаковы.
fn normalize(value: Option<&str>) -> Option<String> {
    value.filter(|v| !v.is_empty()).map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Аргументы
// ---------------------------------------------------------------------------

fn arg_str<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(ToolError::new(format!("`{name}` must be a string"))),
    }
}

fn arg_bool(args: &Value, name: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(ToolError::new(format!("`{name}` must be a boolean"))),
    }
}

/// Идентификатор ресурса для подстановки в путь: только буквы, цифры, `-` и `_`.
fn check_id<'a>(name: &str, id: &'a str) -> Result<&'a str, ToolError> {
    let valid = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(id)
    } else {
        Err(ToolError::new(format!(
            "`{name}` is not a valid App Store Connect ID: `{}`",
            preview(id)
        )))
    }
}

fn require_id<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    let id = arg_str(args, name)?.ok_or_else(|| ToolError::new(format!("`{name}` is required")))?;
    check_id(name, id)
}

fn optional_id<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    arg_str(args, name)?
        .map(|id| check_id(name, id))
        .transpose()
}

/// Имя локали из имени файла: `en-US`, `zh-Hans`, `es-419`.
fn is_locale(name: &str) -> bool {
    (2..=20).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

// ---------------------------------------------------------------------------
// Запросы
// ---------------------------------------------------------------------------

fn json_body(response: ApiResponse) -> Result<(Value, Option<String>), ToolError> {
    match response.body {
        ResponseBody::Json(value) => Ok((value, response.pagination.next)),
        _ => Err(ToolError::new(
            "App Store Connect returned an unexpected (non-JSON) response",
        )),
    }
}

/// Все записи `data` с учётом выдачи по страницам (`links.next`).
async fn fetch_all(ctx: &Context, request: ApiRequest) -> Result<Vec<Value>, ToolError> {
    let mut items = Vec::new();
    let mut response = ctx.api.execute(request).await?;
    for _ in 0..MAX_PAGES {
        let (body, next) = json_body(response)?;
        if let Some(Value::Array(data)) = body.get("data") {
            items.extend(data.iter().cloned());
        }
        let Some(next) = next else {
            return Ok(items);
        };
        response = ctx.api.execute_next(&next).await?;
    }
    Err(ToolError::new(format!(
        "App Store Connect returned more than {MAX_PAGES} pages; the list was not read completely"
    )))
}

async fn fetch_one(ctx: &Context, request: ApiRequest) -> Result<Value, ToolError> {
    let (body, _) = json_body(ctx.api.execute(request).await?)?;
    body.get("data")
        .cloned()
        .filter(Value::is_object)
        .ok_or_else(|| ToolError::new("App Store Connect returned a response without `data`"))
}

fn attr<'a>(item: &'a Value, name: &str) -> Option<&'a str> {
    item.pointer(&format!("/attributes/{name}"))
        .and_then(Value::as_str)
}

fn item_id(item: &Value) -> Option<&str> {
    item.get("id").and_then(Value::as_str)
}

/// Локализации одного вида у версии или appInfo.
async fn fetch_localizations(
    ctx: &Context,
    kind: Kind,
    parent_id: &str,
) -> Result<RemoteMap, ToolError> {
    let mut fields: Vec<&str> = kind.fields().iter().map(|f| f.name).collect();
    fields.push("locale");
    let request = ApiRequest::new(HttpMethod::Get, kind.list_path(parent_id))
        .with_query(format!("fields[{}]", kind.resource()), fields.join(","))
        .with_query("limit", "200");
    let mut map = RemoteMap::new();
    for item in fetch_all(ctx, request).await? {
        let (Some(id), Some(locale)) = (item_id(&item), attr(&item, "locale")) else {
            continue;
        };
        check_id("localization id", id)?;
        let values = kind
            .fields()
            .iter()
            .map(|f| (f.name, normalize(attr(&item, f.name))))
            .collect();
        map.entry(locale.to_owned()).or_insert(RemoteLoc {
            id: id.to_owned(),
            values,
        });
    }
    Ok(map)
}

/// Версия App Store: id, строка версии, состояние.
#[derive(Debug, Clone)]
struct VersionInfo {
    id: String,
    version_string: String,
    platform: Option<String>,
    state: Option<String>,
}

impl VersionInfo {
    fn from_item(item: &Value) -> Option<Self> {
        Some(Self {
            id: item_id(item)?.to_owned(),
            version_string: attr(item, "versionString").unwrap_or_default().to_owned(),
            platform: attr(item, "platform").map(str::to_owned),
            state: attr(item, "appVersionState")
                .or_else(|| attr(item, "appStoreState"))
                .map(str::to_owned),
        })
    }

    fn editable(&self) -> bool {
        self.state
            .as_deref()
            .is_some_and(|s| EDITABLE_VERSION_STATES.contains(&s))
    }

    fn describe(&self) -> Value {
        json!({
            "id": self.id,
            "version_string": self.version_string,
            "platform": self.platform,
            "state": self.state,
        })
    }
}

/// appInfo: id и состояние.
#[derive(Debug, Clone)]
struct AppInfoInfo {
    id: String,
    state: Option<String>,
}

impl AppInfoInfo {
    fn from_item(item: &Value) -> Option<Self> {
        Some(Self {
            id: item_id(item)?.to_owned(),
            state: attr(item, "state")
                .or_else(|| attr(item, "appStoreState"))
                .map(str::to_owned),
        })
    }

    fn editable(&self) -> bool {
        self.state
            .as_deref()
            .is_some_and(|s| EDITABLE_APP_INFO_STATES.contains(&s))
    }

    fn describe(&self) -> Value {
        json!({ "id": self.id, "state": self.state })
    }
}

const VERSION_FIELDS_QUERY: &str = "versionString,platform,appVersionState,appStoreState";
const APP_INFO_FIELDS_QUERY: &str = "state,appStoreState";

async fn get_version(ctx: &Context, id: &str) -> Result<VersionInfo, ToolError> {
    let request = ApiRequest::new(HttpMethod::Get, format!("/v1/appStoreVersions/{id}"))
        .with_query("fields[appStoreVersions]", VERSION_FIELDS_QUERY);
    let item = fetch_one(ctx, request).await?;
    VersionInfo::from_item(&item)
        .ok_or_else(|| ToolError::new("App Store Connect returned a version without `id`"))
}

async fn get_app_info(ctx: &Context, id: &str) -> Result<AppInfoInfo, ToolError> {
    let request = ApiRequest::new(HttpMethod::Get, format!("/v1/appInfos/{id}"))
        .with_query("fields[appInfos]", APP_INFO_FIELDS_QUERY);
    let item = fetch_one(ctx, request).await?;
    AppInfoInfo::from_item(&item)
        .ok_or_else(|| ToolError::new("App Store Connect returned an app info without `id`"))
}

/// Редактируемая версия платформы: ровно одна, иначе ошибка со списком версий.
async fn pick_version(
    ctx: &Context,
    app_id: &str,
    platform: &str,
) -> Result<VersionInfo, ToolError> {
    let request = ApiRequest::new(
        HttpMethod::Get,
        format!("/v1/apps/{app_id}/appStoreVersions"),
    )
    .with_query("filter[platform]", platform)
    .with_query("fields[appStoreVersions]", VERSION_FIELDS_QUERY)
    .with_query("limit", "200");
    let versions: Vec<VersionInfo> = fetch_all(ctx, request)
        .await?
        .iter()
        .filter_map(VersionInfo::from_item)
        .collect();
    let editable: Vec<&VersionInfo> = versions.iter().filter(|v| v.editable()).collect();
    match editable.as_slice() {
        [one] => Ok((*one).clone()),
        _ => {
            let list: Vec<Value> = versions.iter().map(VersionInfo::describe).collect();
            let reason = if editable.is_empty() {
                format!("the app has no editable App Store version for {platform}")
            } else {
                format!("the app has several editable App Store versions for {platform}")
            };
            Err(ToolError::new(format!(
                "{reason} (editable states: {}); pass `version_id`. Versions: {}",
                EDITABLE_VERSION_STATES.join(", "),
                Value::Array(list)
            )))
        }
    }
}

/// Редактируемая appInfo приложения: ровно одна, иначе ошибка со списком.
async fn pick_app_info(ctx: &Context, app_id: &str) -> Result<AppInfoInfo, ToolError> {
    let request = ApiRequest::new(HttpMethod::Get, format!("/v1/apps/{app_id}/appInfos"))
        .with_query("fields[appInfos]", APP_INFO_FIELDS_QUERY)
        .with_query("limit", "200");
    let infos: Vec<AppInfoInfo> = fetch_all(ctx, request)
        .await?
        .iter()
        .filter_map(AppInfoInfo::from_item)
        .collect();
    let editable: Vec<&AppInfoInfo> = infos.iter().filter(|i| i.editable()).collect();
    match editable.as_slice() {
        [one] => Ok((*one).clone()),
        _ => {
            let list: Vec<Value> = infos.iter().map(AppInfoInfo::describe).collect();
            let reason = if editable.is_empty() {
                "the app has no editable app info"
            } else {
                "the app has several editable app infos"
            };
            Err(ToolError::new(format!(
                "{reason} (editable states: {}); pass `app_info_id`. App infos: {}",
                EDITABLE_APP_INFO_STATES.join(", "),
                Value::Array(list)
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// metadata_export
// ---------------------------------------------------------------------------

fn run_export<'a>(ctx: &'a Context, args: Value) -> ToolFuture<'a> {
    Box::pin(export(ctx, args))
}

async fn export(ctx: &Context, args: Value) -> ToolResult {
    let app_id = require_id(&args, "app_id")?;
    let version_id = optional_id(&args, "version_id")?;
    let app_info_id = optional_id(&args, "app_info_id")?;
    let platform = arg_str(&args, "platform")?.unwrap_or("IOS");
    if !PLATFORMS.contains(&platform) {
        return Err(ToolError::new(format!(
            "`platform` must be one of {}",
            PLATFORMS.join(", ")
        )));
    }
    let dir_arg = arg_str(&args, "dir")?.ok_or_else(|| ToolError::new("`dir` is required"))?;

    // Каталог проверяем до запросов: смешивать два экспорта в одном каталоге нельзя.
    let dir = ctx.files.resolve_write_dir(dir_arg)?;
    let existing = exported_files(&dir)?;
    if !existing.is_empty() {
        return Err(ToolError::new(format!(
            "the directory already contains exported metadata files ({}); use an empty or new directory",
            existing.join(", ")
        )));
    }

    let version = match version_id {
        Some(id) => get_version(ctx, id).await?,
        None => pick_version(ctx, app_id, platform).await?,
    };
    let app_info = match app_info_id {
        Some(id) => get_app_info(ctx, id).await?,
        None => pick_app_info(ctx, app_id).await?,
    };
    let version_locs = fetch_localizations(ctx, Kind::Version, &version.id).await?;
    let info_locs = fetch_localizations(ctx, Kind::Info, &app_info.id).await?;

    let locales: BTreeSet<&String> = version_locs.keys().chain(info_locs.keys()).collect();
    let mut files = Vec::new();
    for locale in &locales {
        if !is_locale(locale) {
            return Err(ToolError::new(format!(
                "App Store Connect returned an unexpected locale `{}`",
                preview(locale)
            )));
        }
        let mut sections = Vec::new();
        if let Some(loc) = version_locs.get(*locale) {
            sections.push((Kind::Version, &loc.values));
        }
        if let Some(loc) = info_locs.get(*locale) {
            sections.push((Kind::Info, &loc.values));
        }
        let name = format!("{locale}.json");
        write_new_file(ctx, &dir, &name, &render_locale(&sections))?;
        files.push(name);
    }

    let meta = json!({
        "format": FORMAT_VERSION,
        "app_id": app_id,
        "platform": version.platform,
        "version_id": version.id,
        "version_string": version.version_string,
        "version_state": version.state,
        "app_info_id": app_info.id,
        "app_info_state": app_info.state,
        "exported_at": rfc3339_now(),
    });
    let meta_text = serde_json::to_string_pretty(&meta)
        .map_err(|e| ToolError::new(format!("could not serialize {META_FILE}: {e}")))?;
    write_new_file(ctx, &dir, META_FILE, &format!("{meta_text}\n"))?;
    files.push(META_FILE.to_owned());

    Ok(json!({
        "dir": dir.display().to_string(),
        "app_id": app_id,
        "version": version.describe(),
        "app_info": app_info.describe(),
        "locales": locales,
        "files": files,
        "next_step": "Edit the locale files, then call metadata_apply with this dir (dry_run first).",
    }))
}

/// Файлы экспорта, которые уже лежат в каталоге (`*.json`).
fn exported_files(dir: &Path) -> Result<Vec<String>, ToolError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| ToolError::new(format!("could not read the directory: {e}")))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| ToolError::new(format!("could not read the directory: {e}")))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".json") {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Новый файл в проверенном каталоге; существующий файл не перезаписывается.
fn write_new_file(ctx: &Context, dir: &Path, name: &str, content: &str) -> Result<(), ToolError> {
    let path = ctx.files.resolve_write(&dir.join(name).to_string_lossy())?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| ToolError::new(format!("could not create {name}: {e}")))?;
    file.write_all(content.as_bytes())
        .map_err(|e| ToolError::new(format!("could not write {name}: {e}")))
}

/// JSON файла локали с фиксированным порядком разделов и полей (как в спецификации).
/// `serde_json::Map` без `preserve_order` сортирует ключи, поэтому текст собираем сами.
fn render_locale(sections: &[(Kind, &Values)]) -> String {
    let quote = |s: &str| Value::String(s.to_owned()).to_string();
    let mut out = String::from("{\n");
    for (si, (kind, values)) in sections.iter().enumerate() {
        out.push_str(&format!("  {}: {{\n", quote(kind.section())));
        let fields = kind.fields();
        for (fi, field) in fields.iter().enumerate() {
            let value = match values.get(field.name).cloned().flatten() {
                Some(v) => quote(&v),
                None => "null".to_owned(),
            };
            let comma = if fi + 1 < fields.len() { "," } else { "" };
            out.push_str(&format!("    {}: {value}{comma}\n", quote(field.name)));
        }
        let comma = if si + 1 < sections.len() { "," } else { "" };
        out.push_str(&format!("  }}{comma}\n"));
    }
    out.push_str("}\n");
    out
}

/// Текущее время UTC в RFC 3339 (`2026-11-07T10:00:00Z`) без внешних зависимостей.
fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    rfc3339(secs)
}

fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Алгоритм civil_from_days (Howard Hinnant): дни от 1970-01-01 → дата.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ---------------------------------------------------------------------------
// metadata_apply: локальные файлы
// ---------------------------------------------------------------------------

/// `_meta.json`.
#[derive(Debug)]
struct Meta {
    version_id: String,
    version_string: String,
    app_info_id: String,
}

/// Файл локали: разделы, которые в нём есть.
type LocalLoc = BTreeMap<Kind, Values>;

/// Каталог метаданных: описание и файлы локалей.
struct LocalDir {
    dir: PathBuf,
    meta: Meta,
    locales: BTreeMap<String, LocalLoc>,
}

fn read_json(path: &Path, name: &str) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{name}: could not read: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{name}: invalid JSON: {e}"))
}

/// Читает каталог и проверяет файлы. Ошибка содержит все найденные проблемы.
fn read_dir(ctx: &Context, dir_arg: &str) -> Result<LocalDir, ToolError> {
    let meta_path = ctx
        .files
        .resolve_read(&Path::new(dir_arg).join(META_FILE).to_string_lossy())
        .map_err(|e| {
            ToolError::new(format!(
                "{e}. `dir` must be a directory written by metadata_export (with {META_FILE})"
            ))
        })?;
    let dir = meta_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| ToolError::new("could not determine the metadata directory"))?;

    let mut problems = Vec::new();
    let meta = match read_json(&meta_path, META_FILE) {
        Ok(value) => parse_meta(&value, &mut problems),
        Err(problem) => {
            problems.push(problem);
            None
        }
    };

    let mut names = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| ToolError::new(format!("could not read the directory: {e}")))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| ToolError::new(format!("could not read the directory: {e}")))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != META_FILE && name.ends_with(".json") {
            names.push(name);
        }
    }
    names.sort();

    let mut locales = BTreeMap::new();
    for name in names {
        let locale = name.trim_end_matches(".json");
        if !is_locale(locale) {
            problems.push(format!(
                "{name}: the file name is not a locale (expected `<locale>.json`, for example `en-US.json`)"
            ));
            continue;
        }
        let path = ctx.files.resolve_read(&dir.join(&name).to_string_lossy())?;
        match read_json(&path, &name) {
            Ok(value) => {
                if let Some(loc) = parse_locale(&name, &value, &mut problems) {
                    locales.insert(locale.to_owned(), loc);
                }
            }
            Err(problem) => problems.push(problem),
        }
    }

    match meta {
        Some(meta) if problems.is_empty() => Ok(LocalDir { dir, meta, locales }),
        _ => Err(problems_error("the metadata files are invalid", &problems)),
    }
}

fn problems_error(what: &str, problems: &[String]) -> ToolError {
    ToolError::new(format!(
        "{what}; nothing was changed in App Store Connect. Problems:\n- {}",
        problems.join("\n- ")
    ))
}

fn parse_meta(value: &Value, problems: &mut Vec<String>) -> Option<Meta> {
    const KNOWN: &[&str] = &[
        "format",
        "app_id",
        "platform",
        "version_id",
        "version_string",
        "version_state",
        "app_info_id",
        "app_info_state",
        "exported_at",
    ];
    let Some(obj) = value.as_object() else {
        problems.push(format!("{META_FILE}: must be a JSON object"));
        return None;
    };
    for key in obj.keys() {
        if !KNOWN.contains(&key.as_str()) {
            problems.push(format!("{META_FILE}: unknown field `{}`", preview(key)));
        }
    }
    if let Some(format) = obj.get("format")
        && format.as_u64() != Some(FORMAT_VERSION)
    {
        problems.push(format!(
            "{META_FILE}: unsupported `format` {format}, expected {FORMAT_VERSION}"
        ));
    }
    let mut get = |key: &str| match obj.get(key).and_then(Value::as_str) {
        Some(v) => Some(v.to_owned()),
        None => {
            problems.push(format!("{META_FILE}: `{key}` must be a string"));
            None
        }
    };
    let version_id = get("version_id");
    let version_string = get("version_string");
    let app_info_id = get("app_info_id");
    let meta = Meta {
        version_id: version_id?,
        version_string: version_string?,
        app_info_id: app_info_id?,
    };
    for (key, id) in [
        ("version_id", &meta.version_id),
        ("app_info_id", &meta.app_info_id),
    ] {
        if let Err(e) = check_id(key, id) {
            problems.push(format!("{META_FILE}: {e}"));
        }
    }
    Some(meta)
}

/// Разбор и проверка файла локали: известные разделы и поля, строки или `null`, длины, URL.
fn parse_locale(name: &str, value: &Value, problems: &mut Vec<String>) -> Option<LocalLoc> {
    let Some(obj) = value.as_object() else {
        problems.push(format!("{name}: must be a JSON object"));
        return None;
    };
    let mut loc = LocalLoc::new();
    for (key, section) in obj {
        let Some(kind) = KINDS.iter().copied().find(|k| k.section() == key) else {
            problems.push(format!(
                "{name}: unknown section `{}` (expected `version` or `appInfo`)",
                preview(key)
            ));
            continue;
        };
        let Some(section) = section.as_object() else {
            problems.push(format!("{name}: `{key}` must be an object"));
            continue;
        };
        let mut values = Values::new();
        for (field_name, field_value) in section {
            let Some(field) = kind.field(field_name) else {
                let known: Vec<&str> = kind.fields().iter().map(|f| f.name).collect();
                problems.push(format!(
                    "{name}: unknown field `{key}.{}` (known: {})",
                    preview(field_name),
                    known.join(", ")
                ));
                continue;
            };
            let text = match field_value {
                Value::Null => None,
                Value::String(s) => Some(s.as_str()),
                _ => {
                    problems.push(format!(
                        "{name}: `{key}.{field_name}` must be a string or null"
                    ));
                    continue;
                }
            };
            if let Some(text) = text {
                check_field(name, key, field, text, problems);
            }
            values.insert(field.name, normalize(text));
        }
        loc.insert(kind, values);
    }
    Some(loc)
}

fn check_field(
    name: &str,
    section: &str,
    field: &FieldDef,
    text: &str,
    problems: &mut Vec<String>,
) {
    let chars = text.chars().count();
    if let Some(max) = field.max_chars
        && chars > max
    {
        problems.push(format!(
            "{name}: `{section}.{}` is {chars} characters long, the limit is {max}",
            field.name
        ));
    }
    if field.url && !text.is_empty() && !looks_like_url(text) {
        problems.push(format!(
            "{name}: `{section}.{}` is not an http(s) URL: `{}`",
            field.name,
            preview(text)
        ));
    }
}

/// Простая проверка URL: схема http(s), непустой хост, без пробелов.
fn looks_like_url(text: &str) -> bool {
    let rest = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"));
    let Some(rest) = rest else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty()
        && !text.chars().any(char::is_whitespace)
        && host
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
}

// ---------------------------------------------------------------------------
// metadata_apply: план
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Create,
    Update,
    Delete,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// Изменение поля: новое значение и прежнее (для превью).
#[derive(Debug, Clone)]
struct FieldChange {
    field: &'static str,
    old: Option<String>,
    new: Option<String>,
}

/// Одна запись: создать, изменить или удалить локализацию.
#[derive(Debug, Clone)]
struct Change {
    locale: String,
    kind: Kind,
    action: Action,
    /// id существующей локализации (для `Update` и `Delete`).
    id: Option<String>,
    fields: Vec<FieldChange>,
}

impl Change {
    fn describe(&self) -> Value {
        let fields: Vec<Value> = self
            .fields
            .iter()
            .map(|f| match self.action {
                Action::Update => json!({
                    "field": f.field,
                    "old": f.old.as_deref().map(preview),
                    "new": f.new.as_deref().map(preview),
                }),
                _ => json!({ "field": f.field, "value": f.new.as_deref().map(preview) }),
            })
            .collect();
        let mut out = json!({
            "locale": self.locale,
            "resource": self.kind.section(),
            "action": self.action.as_str(),
        });
        if self.action != Action::Delete {
            out["fields"] = Value::Array(fields);
        }
        out
    }

    /// Запрос записи; тело `PATCH` содержит только изменённые атрибуты.
    fn request(&self, version_id: &str, app_info_id: &str) -> Result<ApiRequest, ToolError> {
        let attributes = || -> Map<String, Value> {
            self.fields
                .iter()
                .map(|f| {
                    let value = f.new.clone().map_or(Value::Null, Value::String);
                    (f.field.to_owned(), value)
                })
                .collect()
        };
        let id = || {
            self.id
                .as_deref()
                .ok_or_else(|| ToolError::new("internal error: localization id is missing"))
        };
        Ok(match self.action {
            Action::Create => {
                let mut attrs = attributes();
                attrs.retain(|_, v| !v.is_null());
                attrs.insert("locale".to_owned(), Value::String(self.locale.clone()));
                let (rel, parent_type) = self.kind.parent();
                let parent_id = match self.kind {
                    Kind::Version => version_id,
                    Kind::Info => app_info_id,
                };
                ApiRequest::new(HttpMethod::Post, self.kind.collection_path()).with_json(json!({
                    "data": {
                        "type": self.kind.resource(),
                        "attributes": attrs,
                        "relationships": {
                            rel: { "data": { "type": parent_type, "id": parent_id } }
                        }
                    }
                }))
            }
            Action::Update => {
                let id = id()?;
                ApiRequest::new(HttpMethod::Patch, self.kind.item_path(id)).with_json(json!({
                    "data": {
                        "type": self.kind.resource(),
                        "id": id,
                        "attributes": attributes(),
                    }
                }))
            }
            Action::Delete => ApiRequest::new(HttpMethod::Delete, self.kind.item_path(id()?)),
        })
    }
}

/// План применения.
struct Plan {
    changes: Vec<Change>,
    /// Локали в App Store Connect без локального файла.
    missing: Vec<String>,
    unchanged: Vec<String>,
}

fn build_plan(
    local: &BTreeMap<String, LocalLoc>,
    remote: &BTreeMap<Kind, RemoteMap>,
    delete_missing: bool,
) -> Result<Plan, Vec<String>> {
    let mut changes = Vec::new();
    let mut problems = Vec::new();
    let mut unchanged = Vec::new();
    let empty = RemoteMap::new();

    for (locale, loc) in local {
        let before = changes.len();
        for (kind, values) in loc {
            let remote_kind = remote.get(kind).unwrap_or(&empty);
            match remote_kind.get(locale) {
                Some(current) => {
                    let mut fields = Vec::new();
                    for field in kind.fields() {
                        let Some(new) = values.get(field.name) else {
                            continue;
                        };
                        let old = current.values.get(field.name).cloned().flatten();
                        if *new != old {
                            if *kind == Kind::Info && field.name == "name" && new.is_none() {
                                problems.push(format!(
                                    "{locale}.json: `appInfo.name` cannot be cleared"
                                ));
                                continue;
                            }
                            fields.push(FieldChange {
                                field: field.name,
                                old,
                                new: new.clone(),
                            });
                        }
                    }
                    if !fields.is_empty() {
                        changes.push(Change {
                            locale: locale.clone(),
                            kind: *kind,
                            action: Action::Update,
                            id: Some(current.id.clone()),
                            fields,
                        });
                    }
                }
                None => {
                    if *kind == Kind::Info && values.get("name").cloned().flatten().is_none() {
                        problems.push(format!(
                            "{locale}.json: `appInfo.name` is required to create the app info localization"
                        ));
                        continue;
                    }
                    let fields = kind
                        .fields()
                        .iter()
                        .filter_map(|f| {
                            values.get(f.name).cloned().flatten().map(|v| FieldChange {
                                field: f.name,
                                old: None,
                                new: Some(v),
                            })
                        })
                        .collect();
                    changes.push(Change {
                        locale: locale.clone(),
                        kind: *kind,
                        action: Action::Create,
                        id: None,
                        fields,
                    });
                }
            }
        }
        if changes.len() == before {
            unchanged.push(locale.clone());
        }
    }

    let mut missing = BTreeSet::new();
    for kind in KINDS {
        for (locale, current) in remote.get(&kind).unwrap_or(&empty) {
            if local.contains_key(locale) {
                continue;
            }
            missing.insert(locale.clone());
            if delete_missing {
                changes.push(Change {
                    locale: locale.clone(),
                    kind,
                    action: Action::Delete,
                    id: Some(current.id.clone()),
                    fields: Vec::new(),
                });
            }
        }
    }
    // Удаления — после создания и правок; внутри — по локали.
    changes.sort_by(|a, b| {
        (a.action == Action::Delete, &a.locale, a.kind).cmp(&(
            b.action == Action::Delete,
            &b.locale,
            b.kind,
        ))
    });

    if problems.is_empty() {
        Ok(Plan {
            changes,
            missing: missing.into_iter().collect(),
            unchanged,
        })
    } else {
        Err(problems)
    }
}

// ---------------------------------------------------------------------------
// metadata_apply
// ---------------------------------------------------------------------------

fn run_apply<'a>(ctx: &'a Context, args: Value) -> ToolFuture<'a> {
    Box::pin(apply(ctx, args))
}

async fn apply(ctx: &Context, args: Value) -> ToolResult {
    let dir_arg = arg_str(&args, "dir")?.ok_or_else(|| ToolError::new("`dir` is required"))?;
    let dry_run = arg_bool(&args, "dry_run", true)?;
    let delete_missing = arg_bool(&args, "delete_missing_locales", false)?;
    let version_override = optional_id(&args, "version_id")?;
    let app_info_override = optional_id(&args, "app_info_id")?;

    // Уровень проверяем до любых запросов: удаление локалей — destructive.
    if delete_missing && !dry_run && !ctx.access.allows(AccessLevel::Destructive) {
        return Err(ToolError::new(
            "`delete_missing_locales` deletes localizations and needs the destructive access level, \
             but this server runs with a lower one. Ask the user to allow `destructive` access for \
             the server's profile (`appstore-connect-mcp ui`), or call again without `delete_missing_locales`. Nothing was sent.",
        ));
    }

    let local = read_dir(ctx, dir_arg)?;

    let version_id = version_override.unwrap_or(&local.meta.version_id);
    let version = get_version(ctx, version_id).await?;
    if version_override.is_none() {
        if version.version_string != local.meta.version_string {
            return Err(ToolError::new(format!(
                "the files were exported for version {} but App Store Connect version {} is now {}; \
                 pass `version_id` to apply them to it anyway. Nothing was sent.",
                local.meta.version_string, version.id, version.version_string
            )));
        }
        if !version.editable() {
            return Err(ToolError::new(format!(
                "App Store version {} ({}) is in state {} and its metadata cannot be edited \
                 (editable states: {}); pass `version_id` to target another version or to try \
                 anyway. Nothing was sent.",
                version.version_string,
                version.id,
                version.state.as_deref().unwrap_or("unknown"),
                EDITABLE_VERSION_STATES.join(", ")
            )));
        }
    }
    let app_info_id = app_info_override.unwrap_or(&local.meta.app_info_id);
    let app_info = get_app_info(ctx, app_info_id).await?;
    if app_info_override.is_none() && !app_info.editable() {
        return Err(ToolError::new(format!(
            "app info {} is in state {} and cannot be edited (editable states: {}); pass \
             `app_info_id` to target another app info or to try anyway. Nothing was sent.",
            app_info.id,
            app_info.state.as_deref().unwrap_or("unknown"),
            EDITABLE_APP_INFO_STATES.join(", ")
        )));
    }

    let mut remote = BTreeMap::new();
    remote.insert(
        Kind::Version,
        fetch_localizations(ctx, Kind::Version, &version.id).await?,
    );
    remote.insert(
        Kind::Info,
        fetch_localizations(ctx, Kind::Info, &app_info.id).await?,
    );

    let plan = build_plan(&local.locales, &remote, delete_missing)
        .map_err(|problems| problems_error("the metadata cannot be applied", &problems))?;

    let mut report = json!({
        "dry_run": dry_run,
        "dir": local.dir.display().to_string(),
        "version": version.describe(),
        "app_info": app_info.describe(),
        "summary": summary(&plan),
        "missing_locales": plan.missing,
        "unchanged_locales": plan.unchanged,
    });
    if version_override.is_some_and(|id| id != local.meta.version_id) {
        report["version_overridden_from"] = json!(local.meta.version_id);
    }
    if app_info_override.is_some_and(|id| id != local.meta.app_info_id) {
        report["app_info_overridden_from"] = json!(local.meta.app_info_id);
    }
    if !plan.missing.is_empty() && !delete_missing {
        report["missing_locales_note"] = json!(
            "These locales exist in App Store Connect but have no file; they are kept. \
             Pass delete_missing_locales: true to delete them."
        );
    }

    if dry_run {
        report["changes"] = plan.changes.iter().map(Change::describe).collect();
        if !plan.changes.is_empty() {
            report["next_step"] =
                json!("Review the changes, then call metadata_apply again with dry_run: false.");
        }
        return Ok(report);
    }

    // Записи независимы (разные локализации): при ошибке продолжаем и сообщаем итог.
    let mut results = Vec::new();
    let (mut applied, mut failed) = (0usize, 0usize);
    for change in &plan.changes {
        let mut entry = change.describe();
        let outcome = match change.request(&version.id, &app_info.id) {
            Ok(request) => ctx.api.execute(request).await.map_err(ToolError::from),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(_) => {
                applied += 1;
                entry["status"] = json!("applied");
            }
            Err(e) => {
                failed += 1;
                entry["status"] = json!("failed");
                entry["error"] = json!(ctx.api.scrub_token(e.message));
            }
        }
        results.push(entry);
    }
    report["changes"] = Value::Array(results);
    report["applied"] = json!(applied);
    report["failed"] = json!(failed);
    if failed > 0 {
        return Err(ToolError::new(format!(
            "metadata_apply partially failed: {applied} of {} changes applied, {failed} failed. \
             Call metadata_apply with dry_run: true to see what is left. Report: {report}",
            plan.changes.len()
        )));
    }
    Ok(report)
}

fn summary(plan: &Plan) -> Value {
    let count = |action: Action| plan.changes.iter().filter(|c| c.action == action).count();
    let fields: usize = plan
        .changes
        .iter()
        .filter(|c| c.action == Action::Update)
        .map(|c| c.fields.len())
        .sum();
    json!({
        "create": count(Action::Create),
        "update": count(Action::Update),
        "fields_changed": fields,
        "delete": count(Action::Delete),
        "missing_locally": plan.missing.len(),
        "unchanged_locales": plan.unchanged.len(),
        "write_requests": plan.changes.len(),
    })
}

/// Короткое превью текста: не длиннее [`PREVIEW_CHARS`] символов, переводы строк — `⏎`.
fn preview(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { '⏎' } else { c })
        .collect();
    if flat.chars().count() <= PREVIEW_CHARS {
        flat
    } else {
        let mut short: String = flat.chars().take(PREVIEW_CHARS - 1).collect();
        short.push('…');
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formats_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
    }

    #[test]
    fn preview_is_limited() {
        let long = "й".repeat(200);
        let p = preview(&long);
        assert_eq!(p.chars().count(), PREVIEW_CHARS);
        assert!(p.ends_with('…'));
        assert_eq!(preview("a\nb"), "a⏎b");
    }

    #[test]
    fn urls_and_locales() {
        assert!(looks_like_url("https://example.com/privacy"));
        assert!(looks_like_url("http://example.com"));
        assert!(!looks_like_url("example.com"));
        assert!(!looks_like_url("https://"));
        assert!(!looks_like_url("https://exa mple.com"));
        assert!(is_locale("en-US"));
        assert!(is_locale("es-419"));
        assert!(!is_locale("../x"));
        assert!(!is_locale("_meta"));
    }

    #[test]
    fn lengths_are_counted_in_characters() {
        let mut problems = Vec::new();
        // 30 символов кириллицы — 60 байт, но в лимит укладываются.
        let value = json!({ "appInfo": { "name": "я".repeat(30) } });
        parse_locale("ru.json", &value, &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
        let value = json!({ "appInfo": { "name": "я".repeat(31) } });
        parse_locale("ru.json", &value, &mut problems);
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn rendered_locale_is_valid_json_in_spec_order() {
        let mut values = Values::new();
        values.insert("whatsNew", Some("Line 1\n\"quoted\"".to_owned()));
        values.insert("description", Some("Desc".to_owned()));
        let text = render_locale(&[(Kind::Version, &values)]);
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["version"]["whatsNew"], "Line 1\n\"quoted\"");
        assert_eq!(parsed["version"]["keywords"], Value::Null);
        let d = text.find("description").unwrap();
        let w = text.find("whatsNew").unwrap();
        assert!(d < w);
    }
}
