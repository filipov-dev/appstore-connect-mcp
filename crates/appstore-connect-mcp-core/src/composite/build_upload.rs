//! `build_upload`: загрузка сборки (`.ipa`, `.pkg`) через Build Upload API App Store Connect.
//!
//! Порядок по спецификации:
//!
//! 1. Версия и номер сборки: из `Payload/<имя>.app/Info.plist` внутри `.ipa`
//!    (`CFBundleShortVersionString`, `CFBundleVersion`, двоичный или XML plist), либо из
//!    аргументов `version` и `build_number` (они важнее архива; расхождение — предупреждение).
//!    `.pkg` (xar) не разбирается: для него оба аргумента обязательны.
//! 2. Платформа: аргумент `platform`, иначе `CFBundleSupportedPlatforms` из `Info.plist`,
//!    иначе по расширению (`.ipa` → `IOS`, `.pkg` → `MAC_OS`).
//! 3. Проверка `CFBundleIdentifier` против `bundleId` приложения (`GET /v1/apps/{id}`):
//!    расхождение и сбой проверки — только предупреждение, решает App Store Connect.
//! 4. `POST /v1/buildUploads` (версия, номер, платформа, связь `app`) →
//!    `POST /v1/buildUploadFiles` (`assetType: ASSET`, `uti`, `fileName`, `fileSize`,
//!    связь `buildUpload`) → части по `uploadOperations` ([`super::upload`]) →
//!    `PATCH /v1/buildUploadFiles/{id}` с `uploaded: true` и
//!    `sourceFileChecksums.file = {hash: MD5, algorithm: MD5}`.
//! 5. При `wait` — опрос `GET /v1/buildUploads/{id}?include=build` до `COMPLETE` или
//!    `FAILED`; пока загрузка в `AWAITING_UPLOAD`, проверяется и `assetDeliveryState`
//!    файла (ошибка контрольной суммы видна только там).
//!
//! Ошибки: сбой создания файла, части или фиксации — `buildUpload` удаляется
//! (`DELETE /v1/buildUploads/{id}`; у `buildUploadFiles` удаления нет). `FAILED` при
//! обработке (подпись, повтор номера сборки и т. п.) — ошибка с текстами `errors` Apple;
//! запись загрузки остаётся, ответ называет инструмент удаления. Срок ожидания (только
//! на обработку, сама передача частей в него не входит) вышел — не ошибка: ответ
//! с состоянием и инструментом для проверки.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use appstore_connect_api::AccessLevel;
use appstore_connect_api::executor::{ApiRequest, HttpMethod};
use serde_json::{Map, Value, json};

use super::upload::{
    self, PollSchedule, data_id, delete_reservation, instance_path, json_body, state_messages,
    str_arg, timeout_arg, tool_for,
};
use super::{CompositeTool, Context, ToolError, ToolFuture};

/// Срок ожидания обработки по умолчанию, секунд.
const DEFAULT_TIMEOUT: u64 = 1800;
/// Наибольший срок ожидания, секунд.
const MAX_TIMEOUT: u64 = 7200;

/// Платформы `buildUploads` (схема `Platform` спецификации).
const PLATFORMS: &[&str] = &["IOS", "MAC_OS", "TV_OS", "VISION_OS"];

/// Наибольший `Info.plist`, который читается из архива (защита от zip-бомбы).
const MAX_PLIST_BYTES: u64 = 8 * 1024 * 1024;

const GET_UPLOAD: &str = "buildUploads_getInstance";
const DELETE_UPLOAD: &str = "buildUploads_deleteInstance";
const GET_BUILD: &str = "builds_getInstance";

#[cfg(not(test))]
const SCHEDULE: PollSchedule = PollSchedule {
    first: Duration::from_secs(10),
    max: Duration::from_secs(60),
};
#[cfg(test)]
const SCHEDULE: PollSchedule = PollSchedule {
    first: Duration::from_millis(5),
    max: Duration::from_millis(20),
};

/// Инструменты модуля.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: "build_upload",
        toolset: "builds",
        access: AccessLevel::Write,
        description: "Upload an app build (.ipa or .pkg) to App Store Connect with the Build \
            Upload API, the replacement for Transporter and altool. The tool reads the version \
            and build number from the .ipa (Payload/*.app/Info.plist), checks the bundle ID \
            against the app, creates the build upload, uploads the file in parts, commits it with \
            its MD5 checksum and (with wait, the default) waits until Apple finishes processing. \
            Returns the build upload id, version, build number, platform, state, the build id \
            once Apple links it, and hints. Processing errors (invalid signature, a build number \
            that was already used) are returned as errors with Apple's text. For .pkg files pass \
            version and build_number. The file must be signed for distribution; this tool does \
            not sign or export archives.",
        input_schema,
        uses_local_files: true,
        run,
    }]
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file": {
                "type": "string",
                "description": "Path of the local .ipa or .pkg (inside the allowed directories).",
            },
            "app_id": {
                "type": "string",
                "description": "App Store Connect app id (apps resource id, not the bundle ID).",
            },
            "platform": {
                "type": "string",
                "enum": PLATFORMS,
                "description": "Platform of the build. Default: from CFBundleSupportedPlatforms \
                    of the .ipa, otherwise IOS for .ipa and MAC_OS for .pkg.",
            },
            "version": {
                "type": "string",
                "description": "CFBundleShortVersionString (for example 1.2.0). Default: read \
                    from the .ipa; required for .pkg.",
            },
            "build_number": {
                "type": "string",
                "description": "CFBundleVersion (for example 42). Default: read from the .ipa; \
                    required for .pkg.",
            },
            "wait": {
                "type": "boolean",
                "default": true,
                "description": "Wait until Apple finishes processing the upload (COMPLETE or FAILED).",
            },
            "timeout_seconds": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_TIMEOUT,
                "default": DEFAULT_TIMEOUT,
                "description": "How long to wait for processing after the file is uploaded. When \
                    it runs out, the tool returns the current state and the tool to check it later.",
            },
        },
        "required": ["file", "app_id"],
        "additionalProperties": false,
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(upload_build(ctx, args))
}

/// Вид архива сборки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Archive {
    Ipa,
    Pkg,
}

impl Archive {
    fn from_path(path: &Path) -> Result<Self, ToolError> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("ipa") => Ok(Self::Ipa),
            Some("pkg") => Ok(Self::Pkg),
            _ => Err(ToolError::new(
                "build_upload accepts only .ipa (iOS, tvOS, visionOS) and .pkg (macOS) files",
            )),
        }
    }

    fn uti(self) -> &'static str {
        match self {
            Self::Ipa => "com.apple.ipa",
            Self::Pkg => "com.apple.pkg",
        }
    }

    fn default_platform(self) -> &'static str {
        match self {
            Self::Ipa => "IOS",
            Self::Pkg => "MAC_OS",
        }
    }
}

/// Данные `Info.plist` приложения.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct BundleInfo {
    version: Option<String>,
    build: Option<String>,
    bundle_id: Option<String>,
    platform: Option<&'static str>,
}

/// Читает `Payload/<имя>.app/Info.plist` из `.ipa`. Вложенные `Info.plist` (фреймворки,
/// расширения) пропускаются: нужен ровно третий уровень пути.
fn read_ipa_info(path: &Path) -> Result<BundleInfo, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("could not open the file: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("the .ipa is not a valid zip: {e}"))?;
    // Имена записей, которые не разобрались как UTF-8, пропускаются: путь приложения
    // в .ipa всегда ASCII/UTF-8.
    let name = archive
        .file_names()
        .filter_map(Result::ok)
        .find(|name| {
            let parts: Vec<&str> = name.split('/').collect();
            matches!(parts.as_slice(), ["Payload", app, "Info.plist"] if app.ends_with(".app"))
        })
        .map(|name| name.into_owned())
        .ok_or_else(|| "Payload/<App>.app/Info.plist was not found in the .ipa".to_owned())?;
    let entry = archive
        .by_name(&name)
        .map_err(|e| format!("could not read {name}: {e}"))?;
    if entry.size() > MAX_PLIST_BYTES {
        return Err(format!("{name} is larger than {MAX_PLIST_BYTES} bytes"));
    }
    let mut bytes = Vec::new();
    entry
        .take(MAX_PLIST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("could not read {name}: {e}"))?;
    if bytes.len() as u64 > MAX_PLIST_BYTES {
        return Err(format!("{name} is larger than {MAX_PLIST_BYTES} bytes"));
    }
    let plist = plist::Value::from_reader(std::io::Cursor::new(bytes))
        .map_err(|e| format!("{name} is not a valid property list: {e}"))?;
    let dict = plist
        .as_dictionary()
        .ok_or_else(|| format!("{name} is not a dictionary"))?;
    let text = |key: &str| {
        dict.get(key)
            .and_then(plist::Value::as_string)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let platform = dict
        .get("CFBundleSupportedPlatforms")
        .and_then(plist::Value::as_array)
        .and_then(|items| items.iter().find_map(plist::Value::as_string))
        .and_then(|platform| match platform {
            "iPhoneOS" => Some("IOS"),
            "AppleTVOS" => Some("TV_OS"),
            "XROS" => Some("VISION_OS"),
            _ => None,
        });
    Ok(BundleInfo {
        version: text("CFBundleShortVersionString"),
        build: text("CFBundleVersion"),
        bundle_id: text("CFBundleIdentifier"),
        platform,
    })
}

async fn upload_build(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let app_id = upload::required_str(&args, "app_id")?.to_owned();
    let wait = args.get("wait").and_then(Value::as_bool).unwrap_or(true);
    let timeout = timeout_arg(&args, DEFAULT_TIMEOUT, MAX_TIMEOUT)?;
    let file = upload::resolve_file(ctx, &args)?;
    let archive = Archive::from_path(&file)?;
    let file_name = upload::file_name(None, &file)?;
    let mut warnings: Vec<String> = Vec::new();

    let version_arg = str_arg(&args, "version");
    let build_arg = str_arg(&args, "build_number");
    let info = match archive {
        Archive::Pkg => BundleInfo::default(),
        Archive::Ipa => {
            let path = file.clone();
            let parsed = tokio::task::spawn_blocking(move || read_ipa_info(&path))
                .await
                .map_err(|_| ToolError::new("reading the .ipa stopped unexpectedly"))?;
            match parsed {
                Ok(info) => info,
                Err(reason) if version_arg.is_some() && build_arg.is_some() => {
                    warnings.push(format!("Could not read Info.plist: {reason}."));
                    BundleInfo::default()
                }
                Err(reason) => {
                    return Err(ToolError::new(format!(
                        "could not read the version from the .ipa: {reason}. Pass version and \
                         build_number (CFBundleShortVersionString and CFBundleVersion)"
                    )));
                }
            }
        }
    };
    let version = pick(
        version_arg,
        info.version.as_deref(),
        "version",
        "CFBundleShortVersionString",
        &mut warnings,
    );
    let build_number = pick(
        build_arg,
        info.build.as_deref(),
        "build_number",
        "CFBundleVersion",
        &mut warnings,
    );
    let (Some(version), Some(build_number)) = (version, build_number) else {
        return Err(ToolError::new(match archive {
            Archive::Pkg => "the version of a .pkg is not read from the file; pass version and \
                build_number (CFBundleShortVersionString and CFBundleVersion of the app)"
                .to_owned(),
            Archive::Ipa => "Info.plist has no CFBundleShortVersionString or CFBundleVersion; \
                pass version and build_number"
                .to_owned(),
        }));
    };
    let platform = match str_arg(&args, "platform") {
        Some(p) if PLATFORMS.contains(&p) => p.to_owned(),
        Some(p) => {
            return Err(ToolError::new(format!(
                "unsupported platform `{p}`; use one of {}",
                PLATFORMS.join(", ")
            )));
        }
        None => info
            .platform
            .unwrap_or(archive.default_platform())
            .to_owned(),
    };

    if let Some(bundle_id) = &info.bundle_id
        && let Some(warning) = check_bundle_id(ctx, &app_id, bundle_id).await
    {
        warnings.push(warning);
    }

    let file_size = upload::file_size(&file).await?;
    let checksum = upload::file_md5(&file).await?;

    // Запись загрузки сборки.
    let body = json!({"data": {
        "type": "buildUploads",
        "attributes": {
            "cfBundleShortVersionString": version,
            "cfBundleVersion": build_number,
            "platform": platform,
        },
        "relationships": {"app": {"data": {"type": "apps", "id": app_id}}},
    }});
    let created = ctx
        .api
        .execute(ApiRequest::new(HttpMethod::Post, "/v1/buildUploads").with_json(body))
        .await
        .map_err(|e| ToolError::new(format!("could not create the build upload: {e}")))?;
    let upload_id = data_id(&json_body(created, "the build upload")?, "the build upload")?;
    let upload_path = instance_path("/v1/buildUploads", &upload_id)?;
    let label = format!("build upload {upload_id}");

    let file_id = match send_file(
        ctx, &upload_id, &file, &file_name, file_size, archive, &checksum,
    )
    .await
    {
        Ok(id) => id,
        Err(reason) => {
            let cleanup = delete_reservation(ctx, &upload_path, &label).await;
            return Err(ToolError::new(format!(
                "Uploading {file_name} failed: {reason}. {cleanup}"
            )));
        }
    };

    // Состояние: один запрос без ожидания, опрос до срока — с ожиданием.
    let wait_for = if wait { timeout } else { Duration::ZERO };
    let polled = upload::poll(wait_for, SCHEDULE, || {
        check_state(ctx, &upload_path, &file_id)
    })
    .await
    .map_err(|e| {
        ToolError::new(format!(
            "{file_name} was uploaded as {label}, but checking its processing state \
                 failed: {e}. Check it later with {}.",
            tool_for(GET_UPLOAD).unwrap_or("api_request")
        ))
    })?;
    let state = polled.last;

    if state.failed() {
        let errors = state.errors.join("; ");
        return Err(ToolError::new(format!(
            "App Store Connect rejected build {version} ({build_number}) for {platform}, {label}: \
             {}. Fix the build (for a used build number, increase CFBundleVersion) and upload \
             it again. The failed upload record is kept; it can be deleted with {} (id {upload_id}).",
            if errors.is_empty() {
                "processing failed without details"
            } else {
                &errors
            },
            tool_for(DELETE_UPLOAD).unwrap_or("api_request"),
        )));
    }

    let mut hints: Vec<String> = Vec::new();
    let get_upload = tool_for(GET_UPLOAD).unwrap_or("api_request");
    if !polled.finished {
        hints.push(if wait {
            format!(
                "Apple has not finished processing the build within the timeout. Check the \
                 state later with {get_upload} (id {upload_id}, include build)."
            )
        } else {
            format!(
                "Processing continues on Apple's side. Check the state with {get_upload} (id \
                 {upload_id}, include build)."
            )
        });
    }
    if let Some(build_id) = &state.build_id {
        hints.push(format!(
            "The build id is {build_id}; inspect it with {} and use it for TestFlight groups \
             or an App Store version.",
            tool_for(GET_BUILD).unwrap_or("api_request")
        ));
    } else if polled.finished {
        hints.push(format!(
            "Apple has not linked a build yet; it appears shortly. Check with {get_upload} \
             (id {upload_id}, include build)."
        ));
    }

    let mut result = json!({
        "buildUploadId": upload_id,
        "buildUploadFileId": file_id,
        "appId": app_id,
        "version": version,
        "buildNumber": build_number,
        "platform": platform,
        "fileName": file_name,
        "fileSize": file_size,
        "sourceFileChecksum": checksum,
        "state": state.state,
        "finished": polled.finished,
    });
    if let Some(bundle_id) = info.bundle_id {
        result["bundleId"] = json!(bundle_id);
    }
    if let Some(build_id) = state.build_id {
        result["buildId"] = json!(build_id);
    }
    if let Some(processing) = state.build_processing {
        result["buildProcessingState"] = json!(processing);
    }
    warnings.extend(state.warnings);
    if !warnings.is_empty() {
        result["warnings"] = json!(warnings);
    }
    if !state.infos.is_empty() {
        result["infos"] = json!(state.infos);
    }
    if !hints.is_empty() {
        result["hints"] = json!(hints);
    }
    Ok(result)
}

/// Значение из аргумента или архива; расхождение — предупреждение.
fn pick(
    arg: Option<&str>,
    archive: Option<&str>,
    arg_name: &str,
    key: &str,
    warnings: &mut Vec<String>,
) -> Option<String> {
    match (arg, archive) {
        (Some(a), Some(b)) if a != b => {
            warnings.push(format!(
                "{arg_name} `{a}` differs from {key} `{b}` in the archive; App Store Connect \
                 validates the uploaded binary and may reject the build."
            ));
            Some(a.to_owned())
        }
        (Some(a), _) => Some(a.to_owned()),
        (None, b) => b.map(str::to_owned),
    }
}

/// Сверяет `CFBundleIdentifier` с `bundleId` приложения. `Some` — текст предупреждения.
async fn check_bundle_id(ctx: &Context, app_id: &str, bundle_id: &str) -> Option<String> {
    let path = match instance_path("/v1/apps", app_id) {
        Ok(path) => path,
        Err(e) => return Some(format!("Could not check the bundle ID: {e}.")),
    };
    let request = ApiRequest::new(HttpMethod::Get, path).with_query("fields[apps]", "bundleId");
    let response = match ctx.api.execute(request).await {
        Ok(response) => response,
        Err(e) => {
            return Some(format!(
                "Could not check the bundle ID of app {app_id}: {e}."
            ));
        }
    };
    let body = json_body(response, "the app").ok()?;
    let expected = body.pointer("/data/attributes/bundleId")?.as_str()?;
    (expected != bundle_id).then(|| {
        format!(
            "CFBundleIdentifier `{bundle_id}` of the archive differs from the bundle ID \
             `{expected}` of app {app_id}; App Store Connect will most likely reject the build."
        )
    })
}

/// Файл загрузки: резервирование, части, фиксация. Возвращает id `buildUploadFiles`.
async fn send_file(
    ctx: &Context,
    upload_id: &str,
    file: &Path,
    file_name: &str,
    file_size: u64,
    archive: Archive,
    checksum: &str,
) -> Result<String, String> {
    let body = json!({"data": {
        "type": "buildUploadFiles",
        "attributes": {
            "assetType": "ASSET",
            "fileName": file_name,
            "fileSize": file_size,
            "uti": archive.uti(),
        },
        "relationships": {"buildUpload": {"data": {"type": "buildUploads", "id": upload_id}}},
    }});
    let created = ctx
        .api
        .execute(ApiRequest::new(HttpMethod::Post, "/v1/buildUploadFiles").with_json(body))
        .await
        .map_err(|e| format!("could not reserve the build file: {e}"))?;
    let created = json_body(created, "the build file").map_err(|e| e.message)?;
    let file_id = data_id(&created, "the build file").map_err(|e| e.message)?;
    let empty = Value::Object(Map::new());
    let reserved = created.pointer("/data/attributes").unwrap_or(&empty);
    let parts = upload::parse_upload_operations(reserved, file_size).map_err(|e| e.message)?;
    let count = parts.len();
    upload::upload_parts(&ctx.external, file, parts).await?;

    let path = instance_path("/v1/buildUploadFiles", &file_id).map_err(|e| e.message)?;
    let body = json!({"data": {
        "type": "buildUploadFiles",
        "id": file_id,
        "attributes": {
            "uploaded": true,
            "sourceFileChecksums": {"file": {"hash": checksum, "algorithm": "MD5"}},
        },
    }});
    ctx.api
        .execute(ApiRequest::new(HttpMethod::Patch, path).with_json(body))
        .await
        .map_err(|e| format!("all {count} parts were sent, but the commit failed: {e}"))?;
    Ok(file_id)
}

/// Состояние загрузки сборки.
struct UploadState {
    state: Option<String>,
    errors: Vec<String>,
    warnings: Vec<String>,
    infos: Vec<String>,
    build_id: Option<String>,
    build_processing: Option<String>,
}

impl UploadState {
    fn failed(&self) -> bool {
        self.state.as_deref() == Some("FAILED")
    }
}

async fn check_state(
    ctx: &Context,
    upload_path: &str,
    file_id: &str,
) -> Result<(UploadState, bool), ToolError> {
    let request = ApiRequest::new(HttpMethod::Get, upload_path).with_query("include", "build");
    let body = json_body(ctx.api.execute(request).await?, "the build upload")?;
    let state_obj = body
        .pointer("/data/attributes/state")
        .cloned()
        .unwrap_or(Value::Null);
    let build_id = body
        .pointer("/data/relationships/build/data/id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let build_processing = build_id.as_deref().and_then(|id| {
        body.get("included")?
            .as_array()?
            .iter()
            .find(|item| item["type"] == "builds" && item["id"] == id)?
            .pointer("/attributes/processingState")?
            .as_str()
            .map(str::to_owned)
    });
    let mut state = UploadState {
        state: state_obj
            .get("state")
            .and_then(Value::as_str)
            .map(str::to_owned),
        errors: state_messages(&state_obj, "errors"),
        warnings: state_messages(&state_obj, "warnings"),
        infos: state_messages(&state_obj, "infos"),
        build_id,
        build_processing,
    };
    // Пока загрузка ждёт файл, ошибка доставки файла (например, неверная контрольная
    // сумма) видна только у самого файла.
    if matches!(state.state.as_deref(), None | Some("AWAITING_UPLOAD")) {
        let path = instance_path("/v1/buildUploadFiles", file_id)?;
        let file = json_body(
            ctx.api
                .execute(ApiRequest::new(HttpMethod::Get, path))
                .await?,
            "the build file",
        )?;
        let delivery = file
            .pointer("/data/attributes/assetDeliveryState")
            .cloned()
            .unwrap_or(Value::Null);
        if delivery.get("state").and_then(Value::as_str) == Some("FAILED") {
            state.state = Some("FAILED".to_owned());
            state.errors.extend(state_messages(&delivery, "errors"));
        }
        state.warnings.extend(state_messages(&delivery, "warnings"));
    }
    let finished = matches!(state.state.as_deref(), Some("COMPLETE" | "FAILED"));
    Ok((state, finished))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use md5::{Digest, Md5};
    use wiremock::matchers::{body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::composite::upload::test_support::{context, payload};

    /// Маленький `.ipa`: `Info.plist` приложения и вложенный `Info.plist` фреймворка с
    /// другой версией (его брать нельзя), плюс «бинарник».
    fn make_ipa(path: &Path, binary_plist: bool, platform: &str) {
        let mut dict = plist::Dictionary::new();
        dict.insert("CFBundleShortVersionString".into(), "1.2.0".into());
        dict.insert("CFBundleVersion".into(), "42".into());
        dict.insert("CFBundleIdentifier".into(), "dev.example.demo".into());
        dict.insert(
            "CFBundleSupportedPlatforms".into(),
            plist::Value::Array(vec![platform.into()]),
        );
        let mut info = Vec::new();
        let value = plist::Value::Dictionary(dict);
        if binary_plist {
            value.to_writer_binary(&mut info).unwrap();
        } else {
            value.to_writer_xml(&mut info).unwrap();
        }
        let mut framework = plist::Dictionary::new();
        framework.insert("CFBundleShortVersionString".into(), "9.9.9".into());
        framework.insert("CFBundleVersion".into(), "999".into());
        let mut framework_bytes = Vec::new();
        plist::Value::Dictionary(framework)
            .to_writer_xml(&mut framework_bytes)
            .unwrap();

        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(
            "Payload/Demo.app/Frameworks/Lib.framework/Info.plist",
            options,
        )
        .unwrap();
        zip.write_all(&framework_bytes).unwrap();
        zip.start_file("Payload/Demo.app/Info.plist", options)
            .unwrap();
        zip.write_all(&info).unwrap();
        zip.start_file("Payload/Demo.app/Demo", options).unwrap();
        zip.write_all(&payload(4000)).unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn reads_info_plist_from_ipa() {
        let dir = tempfile::tempdir().unwrap();
        let ipa = dir.path().join("Demo.ipa");
        make_ipa(&ipa, true, "AppleTVOS");
        let info = read_ipa_info(&ipa).unwrap();
        assert_eq!(
            info,
            BundleInfo {
                version: Some("1.2.0".into()),
                build: Some("42".into()),
                bundle_id: Some("dev.example.demo".into()),
                platform: Some("TV_OS"),
            }
        );
        make_ipa(&ipa, false, "iPhoneOS");
        assert_eq!(read_ipa_info(&ipa).unwrap().platform, Some("IOS"));

        std::fs::write(&ipa, b"not a zip").unwrap();
        assert!(read_ipa_info(&ipa).unwrap_err().contains("not a valid zip"));
    }

    #[test]
    fn hinted_tools_exist() {
        for op in [GET_UPLOAD, DELETE_UPLOAD, GET_BUILD] {
            assert!(tool_for(op).is_some(), "{op}");
        }
        let schema = input_schema();
        assert_eq!(schema["additionalProperties"], false);
    }

    /// Резервирование файла с двумя частями на «хост загрузки» того же `wiremock`.
    fn file_reservation(server: &MockServer, size: usize) -> Value {
        let half = size / 2;
        json!({"data": {
            "type": "buildUploadFiles",
            "id": "file-1",
            "attributes": {
                "assetDeliveryState": {"state": "AWAITING_UPLOAD"},
                "uploadOperations": [
                    {"method": "PUT", "url": format!("{}/bu/p1?X-Amz-Signature=SECRET-SIG", server.uri()),
                     "offset": 0, "length": half, "partNumber": 1,
                     "requestHeaders": [{"name": "Content-Type", "value": "application/octet-stream"}]},
                    {"method": "PUT", "url": format!("{}/bu/p2?X-Amz-Signature=SECRET-SIG", server.uri()),
                     "offset": half, "length": size - half, "partNumber": 2,
                     "requestHeaders": [{"name": "Content-Type", "value": "application/octet-stream"}]},
                ],
            },
        }})
    }

    fn upload_state(state: &str, extra: Value) -> Value {
        let mut body = json!({"data": {
            "type": "buildUploads",
            "id": "bu-1",
            "attributes": {"state": {"state": state}},
        }});
        if let Value::Object(map) = extra {
            for (k, v) in map {
                if k == "state" {
                    for (sk, sv) in v.as_object().unwrap() {
                        body["data"]["attributes"]["state"][sk] = sv.clone();
                    }
                } else {
                    body[k] = v;
                }
            }
        }
        body
    }

    async fn mount_upload(server: &MockServer, data: &[u8]) {
        Mock::given(method("GET"))
            .and(path("/v1/apps/app-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "type": "apps", "id": "app-1", "attributes": {"bundleId": "dev.example.demo"}}})))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/buildUploads"))
            .and(body_partial_json(json!({"data": {
                "type": "buildUploads",
                "attributes": {"cfBundleShortVersionString": "1.2.0", "cfBundleVersion": "42", "platform": "IOS"},
                "relationships": {"app": {"data": {"type": "apps", "id": "app-1"}}},
            }})))
            .respond_with(ResponseTemplate::new(201).set_body_json(upload_state("AWAITING_UPLOAD", json!({}))))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/buildUploadFiles"))
            .and(body_partial_json(json!({"data": {
                "type": "buildUploadFiles",
                "attributes": {"assetType": "ASSET", "uti": "com.apple.ipa", "fileName": "Demo.ipa",
                    "fileSize": data.len()},
                "relationships": {"buildUpload": {"data": {"type": "buildUploads", "id": "bu-1"}}},
            }})))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(file_reservation(server, data.len())),
            )
            .expect(1)
            .mount(server)
            .await;
        let half = data.len() / 2;
        for (name, bytes) in [("/bu/p1", &data[..half]), ("/bu/p2", &data[half..])] {
            Mock::given(method("PUT"))
                .and(path(name))
                .and(wiremock::matchers::body_bytes(bytes.to_vec()))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(server)
                .await;
        }
        let md5 = upload::hex(&Md5::digest(data));
        Mock::given(method("PATCH"))
            .and(path("/v1/buildUploadFiles/file-1"))
            .and(body_partial_json(json!({"data": {"attributes": {
                "uploaded": true,
                "sourceFileChecksums": {"file": {"hash": md5, "algorithm": "MD5"}},
            }}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "type": "buildUploadFiles", "id": "file-1",
                "attributes": {"assetDeliveryState": {"state": "UPLOAD_COMPLETE"}}}})))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn uploads_ipa_and_waits_for_build() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let ipa = dir.path().join("Demo.ipa");
        make_ipa(&ipa, true, "iPhoneOS");
        let data = std::fs::read(&ipa).unwrap();
        let ctx = context(&server.uri(), dir.path());
        mount_upload(&server, &data).await;
        Mock::given(method("GET"))
            .and(path("/v1/buildUploads/bu-1"))
            .and(query_param("include", "build"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(upload_state("PROCESSING", json!({}))),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/buildUploads/bu-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upload_state(
                "COMPLETE",
                json!({
                    "data": {"type": "buildUploads", "id": "bu-1",
                        "attributes": {"state": {"state": "COMPLETE",
                            "infos": [{"code": "INFO", "description": "Processed"}]}},
                        "relationships": {"build": {"data": {"type": "builds", "id": "build-9"}}}},
                    "included": [{"type": "builds", "id": "build-9",
                        "attributes": {"processingState": "PROCESSING"}}],
                }),
            )))
            .mount(&server)
            .await;

        let result = upload_build(&ctx, json!({"file": "Demo.ipa", "app_id": "app-1"}))
            .await
            .unwrap();
        assert_eq!(result["buildUploadId"], "bu-1");
        assert_eq!(result["version"], "1.2.0");
        assert_eq!(result["buildNumber"], "42");
        assert_eq!(result["platform"], "IOS");
        assert_eq!(result["bundleId"], "dev.example.demo");
        assert_eq!(result["state"], "COMPLETE");
        assert_eq!(result["finished"], true);
        assert_eq!(result["buildId"], "build-9");
        assert_eq!(result["buildProcessingState"], "PROCESSING");
        assert_eq!(result["infos"], json!(["INFO: Processed"]));
        assert!(result.get("warnings").is_none(), "{result}");
        let text = result.to_string();
        assert!(!text.contains("SECRET-SIG"), "{text}");
        assert!(!text.contains("/bu/p"), "{text}");
        assert!(text.contains(tool_for(GET_BUILD).unwrap()), "{text}");
    }

    #[tokio::test]
    async fn processing_failure_is_an_error_with_apple_text() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let ipa = dir.path().join("Demo.ipa");
        make_ipa(&ipa, false, "iPhoneOS");
        let data = std::fs::read(&ipa).unwrap();
        let ctx = context(&server.uri(), dir.path());
        mount_upload(&server, &data).await;
        Mock::given(method("GET"))
            .and(path("/v1/buildUploads/bu-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upload_state(
                "FAILED",
                json!({"state": {"errors": [{"code": "90062",
                    "description": "The value for key CFBundleVersion [42] must be higher than the previously uploaded version."}]}}),
            )))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;

        let err = upload_build(&ctx, json!({"file": "Demo.ipa", "app_id": "app-1"}))
            .await
            .unwrap_err();
        let text = err.message;
        assert!(
            text.contains("must be higher than the previously uploaded"),
            "{text}"
        );
        assert!(text.contains("bu-1"), "{text}");
        assert!(text.contains(tool_for(DELETE_UPLOAD).unwrap()), "{text}");
        assert!(!text.contains("SECRET-SIG"), "{text}");
    }

    #[tokio::test]
    async fn failed_part_deletes_build_upload() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let ipa = dir.path().join("Demo.ipa");
        make_ipa(&ipa, true, "iPhoneOS");
        let size = std::fs::metadata(&ipa).unwrap().len() as usize;
        let ctx = context(&server.uri(), dir.path());
        Mock::given(method("GET"))
            .and(path("/v1/apps/app-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "type": "apps", "id": "app-1", "attributes": {"bundleId": "dev.example.other"}}})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/buildUploads"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(upload_state("AWAITING_UPLOAD", json!({}))),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/buildUploadFiles"))
            .respond_with(ResponseTemplate::new(201).set_body_json(file_reservation(&server, size)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/buildUploads/bu-1"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let err = upload_build(&ctx, json!({"file": "Demo.ipa", "app_id": "app-1"}))
            .await
            .unwrap_err();
        let text = err.message;
        assert!(text.contains("HTTP 503"), "{text}");
        assert!(text.contains("after 3 attempts"), "{text}");
        assert!(text.contains("build upload bu-1 was deleted"), "{text}");
        assert!(!text.contains("SECRET-SIG"), "{text}");
        assert!(!text.contains("/bu/p"), "{text}");
    }

    #[tokio::test]
    async fn pkg_requires_version_and_mismatch_warns() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("App.pkg"), payload(100)).unwrap();
        let ctx = context(&server.uri(), dir.path());
        let err = upload_build(&ctx, json!({"file": "App.pkg", "app_id": "app-1"}))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("pass version and build_number"),
            "{err}"
        );

        std::fs::write(dir.path().join("App.zip"), payload(100)).unwrap();
        let err = upload_build(&ctx, json!({"file": "App.zip", "app_id": "app-1"}))
            .await
            .unwrap_err();
        assert!(err.message.contains(".ipa"), "{err}");

        assert_eq!(
            pick(
                Some("2.0"),
                Some("1.0"),
                "version",
                "CFBundleShortVersionString",
                &mut Vec::new()
            ),
            Some("2.0".into())
        );
        let mut warnings = Vec::new();
        pick(
            Some("2.0"),
            Some("1.0"),
            "version",
            "CFBundleShortVersionString",
            &mut warnings,
        );
        assert_eq!(warnings.len(), 1);
    }

    #[tokio::test]
    async fn no_wait_returns_state_and_hint() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let ipa = dir.path().join("Demo.ipa");
        make_ipa(&ipa, true, "iPhoneOS");
        let data = std::fs::read(&ipa).unwrap();
        let ctx = context(&server.uri(), dir.path());
        mount_upload(&server, &data).await;
        Mock::given(method("GET"))
            .and(path("/v1/buildUploads/bu-1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(upload_state("PROCESSING", json!({}))),
            )
            .expect(1)
            .mount(&server)
            .await;
        let result = upload_build(
            &ctx,
            json!({"file": "Demo.ipa", "app_id": "app-1", "wait": false}),
        )
        .await
        .unwrap();
        assert_eq!(result["state"], "PROCESSING");
        assert_eq!(result["finished"], false);
        let hints = result["hints"].to_string();
        assert!(hints.contains(tool_for(GET_UPLOAD).unwrap()), "{hints}");
    }
}
