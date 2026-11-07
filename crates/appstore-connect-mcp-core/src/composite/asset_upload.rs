//! `asset_upload`: загрузка медиа и вложений из локального файла одним вызовом.
//!
//! Поддерживаются все ресурсы спецификации с `uploadOperations`, кроме устаревших
//! (`/v1` изображений Game Center) и файлов сборок (их загружает `build_upload`).
//! Таблица [`ASSETS`] составлена по спецификации вручную; тест сверяет её со снимком
//! спецификации: пути, `operationId`, тип, связи с родителем, атрибуты создания,
//! атрибут контрольной суммы при фиксации, поле состояния и наличие удаления.
//!
//! Порядок: размер и MD5 файла → резервирование (`POST` c `fileName`, `fileSize`, связью
//! с родителем и дополнительными атрибутами ресурса) → части по `uploadOperations`
//! ([`super::upload`]) → фиксация (`PATCH` с `uploaded: true` и контрольной суммой, если
//! ресурс её принимает) → при `wait` опрос `GET` до `COMPLETE` или `FAILED`.
//!
//! Ошибки:
//! - сбой части или фиксации — незавершённое резервирование удаляется (`DELETE`, если
//!   у ресурса есть удаление; иначе ответ говорит, что оно осталось). Удаляется только
//!   ресурс, созданный этим же вызовом, поэтому инструменту хватает уровня `write`;
//! - `FAILED` при обработке — ошибка с текстами `errors` Apple; зафиксированный ресурс
//!   не удаляется (он уже виден в App Store Connect, пользователь может захотеть
//!   посмотреть его), ответ называет id и инструмент удаления;
//! - срок ожидания вышел — не ошибка: ответ с текущим состоянием и подсказкой, каким
//!   инструментом проверить состояние позже.

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
const DEFAULT_TIMEOUT: u64 = 300;
/// Наибольший срок ожидания, секунд.
const MAX_TIMEOUT: u64 = 1800;

#[cfg(not(test))]
const SCHEDULE: PollSchedule = PollSchedule {
    first: Duration::from_secs(2),
    max: Duration::from_secs(15),
};
#[cfg(test)]
const SCHEDULE: PollSchedule = PollSchedule {
    first: Duration::from_millis(5),
    max: Duration::from_millis(20),
};

/// Связь резервирования с родительским ресурсом.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Parent {
    /// Имя связи в `relationships` запроса создания.
    pub relationship: &'static str,
    /// JSON:API-тип родителя.
    pub type_name: &'static str,
}

/// Дополнительный атрибут создания, задаваемый аргументом инструмента.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Extra {
    /// Имя аргумента инструмента.
    pub arg: &'static str,
    /// Имя атрибута в запросе создания.
    pub attribute: &'static str,
    pub required: bool,
    /// Допустимые значения (пусто — любая строка).
    pub values: &'static [&'static str],
}

/// Как ресурс принимает контрольную сумму при фиксации.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Checksum {
    /// Не принимает: в `PATCH` только `uploaded: true`.
    None,
    /// `sourceFileChecksum`: MD5 в hex.
    Md5,
    /// `sourceFileChecksums`: `{"file": {"hash", "algorithm": "MD5"}}`.
    Checksums,
}

/// Где у ресурса состояние обработки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateField {
    /// `assetDeliveryState`: `{state, errors, warnings}`.
    Delivery,
    /// `state`: строка (`AWAITING_UPLOAD`, `UPLOAD_COMPLETE`, `FAILED`, далее — состояния
    /// проверки). Только у изображений покупок и подписок `/v1`.
    Plain,
}

/// Ресурс, который загружает `asset_upload`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AssetKind {
    /// Значение аргумента `resource`: префикс `operationId` операции создания.
    pub resource: &'static str,
    /// Схема ресурса в спецификации (по ней тест сверяет таблицу со снимком).
    #[cfg_attr(not(test), allow(dead_code))]
    pub schema: &'static str,
    /// JSON:API-тип.
    pub type_name: &'static str,
    /// Путь коллекции (`POST`); экземпляр — `{path}/{id}`.
    pub path: &'static str,
    /// Возможные родители; пусто — ресурс создаётся без связи.
    pub parents: &'static [Parent],
    pub extras: &'static [Extra],
    pub checksum: Checksum,
    pub state: StateField,
    /// Есть ли `DELETE {path}/{id}`.
    pub deletable: bool,
    /// Что это (для описания инструмента).
    pub summary: &'static str,
}

impl AssetKind {
    fn get_operation(&self) -> String {
        format!("{}_getInstance", self.resource)
    }

    fn delete_operation(&self) -> String {
        format!("{}_deleteInstance", self.resource)
    }
}

const fn parent(relationship: &'static str, type_name: &'static str) -> Parent {
    Parent {
        relationship,
        type_name,
    }
}

const PREVIEW_FRAME: Extra = Extra {
    arg: "preview_frame_time_code",
    attribute: "previewFrameTimeCode",
    required: false,
    values: &[],
};
const MIME_TYPE: Extra = Extra {
    arg: "mime_type",
    attribute: "mimeType",
    required: false,
    values: &[],
};
const APP_EVENT_ASSET_TYPE: Extra = Extra {
    arg: "app_event_asset_type",
    attribute: "appEventAssetType",
    required: true,
    values: &["EVENT_CARD", "EVENT_DETAILS_PAGE"],
};
const BACKGROUND_ASSET_TYPE: Extra = Extra {
    arg: "asset_type",
    attribute: "assetType",
    required: true,
    values: &["ASSET", "MANIFEST"],
};

/// Все аргументы дополнительных атрибутов (для схемы).
const ALL_EXTRAS: &[Extra] = &[
    PREVIEW_FRAME,
    MIME_TYPE,
    APP_EVENT_ASSET_TYPE,
    BACKGROUND_ASSET_TYPE,
];

/// Ресурсы с `uploadOperations` (по спецификации).
pub(crate) const ASSETS: &[AssetKind] = &[
    AssetKind {
        resource: "appScreenshots",
        schema: "AppScreenshot",
        type_name: "appScreenshots",
        path: "/v1/appScreenshots",
        parents: &[parent("appScreenshotSet", "appScreenshotSets")],
        extras: &[],
        checksum: Checksum::Md5,
        state: StateField::Delivery,
        deletable: true,
        summary: "App Store screenshot; parent_id is an appScreenshotSets id",
    },
    AssetKind {
        resource: "appPreviews",
        schema: "AppPreview",
        type_name: "appPreviews",
        path: "/v1/appPreviews",
        parents: &[parent("appPreviewSet", "appPreviewSets")],
        extras: &[PREVIEW_FRAME, MIME_TYPE],
        checksum: Checksum::Md5,
        state: StateField::Delivery,
        deletable: true,
        summary: "App Store app preview video; parent_id is an appPreviewSets id; optional preview_frame_time_code, mime_type",
    },
    AssetKind {
        resource: "appStoreReviewAttachments",
        schema: "AppStoreReviewAttachment",
        type_name: "appStoreReviewAttachments",
        path: "/v1/appStoreReviewAttachments",
        parents: &[parent("appStoreReviewDetail", "appStoreReviewDetails")],
        extras: &[],
        checksum: Checksum::Md5,
        state: StateField::Delivery,
        deletable: true,
        summary: "attachment for App Review; parent_id is an appStoreReviewDetails id",
    },
    AssetKind {
        resource: "appEncryptionDeclarationDocuments",
        schema: "AppEncryptionDeclarationDocument",
        type_name: "appEncryptionDeclarationDocuments",
        path: "/v1/appEncryptionDeclarationDocuments",
        parents: &[parent(
            "appEncryptionDeclaration",
            "appEncryptionDeclarations",
        )],
        extras: &[],
        checksum: Checksum::Md5,
        state: StateField::Delivery,
        deletable: false,
        summary: "export compliance document; parent_id is an appEncryptionDeclarations id",
    },
    AssetKind {
        resource: "routingAppCoverages",
        schema: "RoutingAppCoverage",
        type_name: "routingAppCoverages",
        path: "/v1/routingAppCoverages",
        parents: &[parent("appStoreVersion", "appStoreVersions")],
        extras: &[],
        checksum: Checksum::Md5,
        state: StateField::Delivery,
        deletable: true,
        summary: "routing app coverage file (.geojson); parent_id is an appStoreVersions id",
    },
    AssetKind {
        resource: "appEventScreenshots",
        schema: "AppEventScreenshot",
        type_name: "appEventScreenshots",
        path: "/v1/appEventScreenshots",
        parents: &[parent("appEventLocalization", "appEventLocalizations")],
        extras: &[APP_EVENT_ASSET_TYPE],
        checksum: Checksum::None,
        state: StateField::Delivery,
        deletable: true,
        summary: "in-app event image; parent_id is an appEventLocalizations id; app_event_asset_type is required",
    },
    AssetKind {
        resource: "appEventVideoClips",
        schema: "AppEventVideoClip",
        type_name: "appEventVideoClips",
        path: "/v1/appEventVideoClips",
        parents: &[parent("appEventLocalization", "appEventLocalizations")],
        extras: &[PREVIEW_FRAME, APP_EVENT_ASSET_TYPE],
        checksum: Checksum::None,
        state: StateField::Delivery,
        deletable: true,
        summary: "in-app event video; parent_id is an appEventLocalizations id; app_event_asset_type is required",
    },
    AssetKind {
        resource: "appClipHeaderImages",
        schema: "AppClipHeaderImage",
        type_name: "appClipHeaderImages",
        path: "/v1/appClipHeaderImages",
        parents: &[parent(
            "appClipDefaultExperienceLocalization",
            "appClipDefaultExperienceLocalizations",
        )],
        extras: &[],
        checksum: Checksum::Md5,
        state: StateField::Delivery,
        deletable: true,
        summary: "App Clip card header image; parent_id is an appClipDefaultExperienceLocalizations id",
    },
    AssetKind {
        resource: "appClipAdvancedExperienceImages",
        schema: "AppClipAdvancedExperienceImage",
        type_name: "appClipAdvancedExperienceImages",
        path: "/v1/appClipAdvancedExperienceImages",
        parents: &[],
        extras: &[],
        checksum: Checksum::Md5,
        state: StateField::Delivery,
        deletable: false,
        summary: "App Clip advanced experience image; no parent_id (attach the image to the advanced experience afterwards)",
    },
    AssetKind {
        resource: "inAppPurchaseAppStoreReviewScreenshots",
        schema: "InAppPurchaseAppStoreReviewScreenshot",
        type_name: "inAppPurchaseAppStoreReviewScreenshots",
        path: "/v1/inAppPurchaseAppStoreReviewScreenshots",
        parents: &[parent("inAppPurchaseV2", "inAppPurchases")],
        extras: &[],
        checksum: Checksum::Md5,
        state: StateField::Delivery,
        deletable: true,
        summary: "in-app purchase review screenshot; parent_id is an inAppPurchases id",
    },
    AssetKind {
        resource: "inAppPurchaseImages",
        schema: "InAppPurchaseImage",
        type_name: "inAppPurchaseImages",
        path: "/v1/inAppPurchaseImages",
        parents: &[parent("inAppPurchase", "inAppPurchases")],
        extras: &[],
        checksum: Checksum::Md5,
        state: StateField::Plain,
        deletable: true,
        summary: "in-app purchase promotional image; parent_id is an inAppPurchases id",
    },
    AssetKind {
        resource: "inAppPurchaseImagesV2",
        schema: "InAppPurchaseImageV2",
        type_name: "inAppPurchaseImages",
        path: "/v2/inAppPurchaseImages",
        parents: &[parent("version", "inAppPurchaseVersions")],
        extras: &[],
        checksum: Checksum::None,
        state: StateField::Delivery,
        deletable: true,
        summary: "in-app purchase image of an in-app purchase version; parent_id is an inAppPurchaseVersions id",
    },
    AssetKind {
        resource: "subscriptionAppStoreReviewScreenshots",
        schema: "SubscriptionAppStoreReviewScreenshot",
        type_name: "subscriptionAppStoreReviewScreenshots",
        path: "/v1/subscriptionAppStoreReviewScreenshots",
        parents: &[parent("subscription", "subscriptions")],
        extras: &[],
        checksum: Checksum::Md5,
        state: StateField::Delivery,
        deletable: true,
        summary: "subscription review screenshot; parent_id is a subscriptions id",
    },
    AssetKind {
        resource: "subscriptionImages",
        schema: "SubscriptionImage",
        type_name: "subscriptionImages",
        path: "/v1/subscriptionImages",
        parents: &[parent("subscription", "subscriptions")],
        extras: &[],
        checksum: Checksum::Md5,
        state: StateField::Plain,
        deletable: true,
        summary: "subscription promotional image; parent_id is a subscriptions id",
    },
    AssetKind {
        resource: "subscriptionImagesV2",
        schema: "SubscriptionImageV2",
        type_name: "subscriptionImages",
        path: "/v2/subscriptionImages",
        parents: &[parent("version", "subscriptionVersions")],
        extras: &[],
        checksum: Checksum::None,
        state: StateField::Delivery,
        deletable: true,
        summary: "subscription image of a subscription version; parent_id is a subscriptionVersions id",
    },
    AssetKind {
        resource: "gameCenterAchievementImagesV2",
        schema: "GameCenterAchievementImageV2",
        type_name: "gameCenterAchievementImages",
        path: "/v2/gameCenterAchievementImages",
        parents: &[parent("localization", "gameCenterAchievementLocalizations")],
        extras: &[],
        checksum: Checksum::None,
        state: StateField::Delivery,
        deletable: true,
        summary: "Game Center achievement image; parent_id is a gameCenterAchievementLocalizations id",
    },
    AssetKind {
        resource: "gameCenterLeaderboardImagesV2",
        schema: "GameCenterLeaderboardImageV2",
        type_name: "gameCenterLeaderboardImages",
        path: "/v2/gameCenterLeaderboardImages",
        parents: &[parent("localization", "gameCenterLeaderboardLocalizations")],
        extras: &[],
        checksum: Checksum::None,
        state: StateField::Delivery,
        deletable: true,
        summary: "Game Center leaderboard image; parent_id is a gameCenterLeaderboardLocalizations id",
    },
    AssetKind {
        resource: "gameCenterLeaderboardSetImagesV2",
        schema: "GameCenterLeaderboardSetImageV2",
        type_name: "gameCenterLeaderboardSetImages",
        path: "/v2/gameCenterLeaderboardSetImages",
        parents: &[parent(
            "localization",
            "gameCenterLeaderboardSetLocalizations",
        )],
        extras: &[],
        checksum: Checksum::None,
        state: StateField::Delivery,
        deletable: true,
        summary: "Game Center leaderboard set image; parent_id is a gameCenterLeaderboardSetLocalizations id",
    },
    AssetKind {
        resource: "gameCenterActivityImages",
        schema: "GameCenterActivityImage",
        type_name: "gameCenterActivityImages",
        path: "/v1/gameCenterActivityImages",
        parents: &[
            parent("localization", "gameCenterActivityLocalizations"),
            parent("version", "gameCenterActivityVersions"),
        ],
        extras: &[],
        checksum: Checksum::None,
        state: StateField::Delivery,
        deletable: true,
        summary: "Game Center activity image; parent_relationship `localization` (gameCenterActivityLocalizations id) or `version` (gameCenterActivityVersions id, default image)",
    },
    AssetKind {
        resource: "gameCenterChallengeImages",
        schema: "GameCenterChallengeImage",
        type_name: "gameCenterChallengeImages",
        path: "/v1/gameCenterChallengeImages",
        parents: &[
            parent("localization", "gameCenterChallengeLocalizations"),
            parent("version", "gameCenterChallengeVersions"),
        ],
        extras: &[],
        checksum: Checksum::None,
        state: StateField::Delivery,
        deletable: true,
        summary: "Game Center challenge image; parent_relationship `localization` (gameCenterChallengeLocalizations id) or `version` (gameCenterChallengeVersions id, default image)",
    },
    AssetKind {
        resource: "backgroundAssetUploadFiles",
        schema: "BackgroundAssetUploadFile",
        type_name: "backgroundAssetUploadFiles",
        path: "/v1/backgroundAssetUploadFiles",
        parents: &[parent("backgroundAssetVersion", "backgroundAssetVersions")],
        extras: &[BACKGROUND_ASSET_TYPE],
        checksum: Checksum::Checksums,
        state: StateField::Delivery,
        deletable: false,
        summary: "Apple-hosted background asset file; parent_id is a backgroundAssetVersions id; asset_type is required",
    },
];

fn find_kind(resource: &str) -> Option<&'static AssetKind> {
    ASSETS.iter().find(|kind| kind.resource == resource)
}

/// Инструменты модуля.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: "asset_upload",
        toolset: "media",
        access: AccessLevel::Write,
        description: "Upload a local file to App Store Connect in one call: screenshots, app \
            previews, App Review attachments, in-app purchase and subscription images and review \
            screenshots, Game Center images, in-app event media, App Clip images, routing app \
            coverage, export compliance documents and background asset files. The tool reserves \
            the asset under parent_id, uploads the file parts, commits it with its MD5 checksum \
            and (with wait, the default) waits until Apple finishes processing. Use it instead of \
            calling the create/update operations of these resources by hand. Returns the asset \
            id, file name, size, checksum and the processing state with Apple's warnings and \
            errors; upload URLs are never returned. If a part or the commit fails, the unfinished \
            reservation is deleted. If processing fails, the asset is kept and the error names it. \
            Builds (.ipa, .pkg) are uploaded with build_upload.",
        input_schema,
        uses_local_files: true,
        run,
    }]
}

fn input_schema() -> Value {
    let resources: Vec<&str> = ASSETS.iter().map(|kind| kind.resource).collect();
    let list: Vec<String> = ASSETS
        .iter()
        .map(|kind| format!("{}: {}", kind.resource, kind.summary))
        .collect();
    let mut properties = Map::new();
    properties.insert(
        "resource".into(),
        json!({
            "type": "string",
            "enum": resources,
            "description": format!("What to upload. {}.", list.join("; ")),
        }),
    );
    properties.insert(
        "parent_id".into(),
        json!({
            "type": "string",
            "description": "ID of the parent resource the asset belongs to (see `resource`). \
                Not used for appClipAdvancedExperienceImages.",
        }),
    );
    properties.insert(
        "parent_relationship".into(),
        json!({
            "type": "string",
            "enum": ["localization", "version"],
            "description": "Only for gameCenterActivityImages and gameCenterChallengeImages: \
                whether parent_id is a localization or a version.",
        }),
    );
    properties.insert(
        "file".into(),
        json!({
            "type": "string",
            "description": "Path of the local file to upload (inside the allowed directories).",
        }),
    );
    properties.insert(
        "file_name".into(),
        json!({
            "type": "string",
            "description": "File name to report to App Store Connect. Default: the name of `file`.",
        }),
    );
    for extra in ALL_EXTRAS {
        let users: Vec<&str> = ASSETS
            .iter()
            .filter(|kind| kind.extras.iter().any(|e| e.arg == extra.arg))
            .map(|kind| kind.resource)
            .collect();
        let mut schema = json!({
            "type": "string",
            "description": format!(
                "`{}` attribute of the asset; only for {}{}.",
                extra.attribute,
                users.join(", "),
                if extra.required { " (required there)" } else { "" },
            ),
        });
        if !extra.values.is_empty() {
            schema["enum"] = json!(extra.values);
        }
        properties.insert(extra.arg.into(), schema);
    }
    properties.insert(
        "wait".into(),
        json!({
            "type": "boolean",
            "default": true,
            "description": "Wait until Apple finishes processing the file (COMPLETE or FAILED).",
        }),
    );
    properties.insert(
        "timeout_seconds".into(),
        json!({
            "type": "integer",
            "minimum": 0,
            "maximum": MAX_TIMEOUT,
            "default": DEFAULT_TIMEOUT,
            "description": "How long to wait for processing. When it runs out, the tool returns \
                the current state and the tool to check it later.",
        }),
    );
    json!({
        "type": "object",
        "properties": properties,
        "required": ["resource", "file"],
        "additionalProperties": false,
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(upload_asset(ctx, args))
}

/// Проверенный запрос на загрузку.
#[derive(Debug)]
struct Plan {
    kind: &'static AssetKind,
    relationships: Option<Value>,
    attributes: Map<String, Value>,
}

fn plan(args: &Value) -> Result<Plan, ToolError> {
    let resource = upload::required_str(args, "resource")?;
    let kind = find_kind(resource).ok_or_else(|| {
        ToolError::new(format!(
            "unsupported resource `{resource}`; use one of: {}",
            ASSETS
                .iter()
                .map(|k| k.resource)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let parent_id = str_arg(args, "parent_id");
    let relationship = str_arg(args, "parent_relationship");
    let relationships = match (kind.parents, parent_id) {
        ([], None) => {
            if relationship.is_some() {
                return Err(ToolError::new(format!(
                    "parent_relationship is not used for {resource}"
                )));
            }
            None
        }
        ([], Some(_)) => {
            return Err(ToolError::new(format!(
                "{resource} is created without a parent; remove parent_id and link the \
                 uploaded image to its parent afterwards"
            )));
        }
        (_, None) => {
            return Err(ToolError::new(format!(
                "parent_id is required for {resource} ({})",
                kind.summary
            )));
        }
        (parents, Some(id)) => {
            let parent = match (parents, relationship) {
                ([single], None) => single,
                ([single], Some(name)) if name == single.relationship => single,
                ([single], Some(name)) => {
                    return Err(ToolError::new(format!(
                        "parent_relationship `{name}` is not valid for {resource}; it only \
                         has `{}`",
                        single.relationship
                    )));
                }
                (_, None) => {
                    return Err(ToolError::new(format!(
                        "parent_relationship is required for {resource}: one of {}",
                        parents
                            .iter()
                            .map(|p| p.relationship)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                (_, Some(name)) => {
                    parents
                        .iter()
                        .find(|p| p.relationship == name)
                        .ok_or_else(|| {
                            ToolError::new(format!(
                                "parent_relationship `{name}` is not valid for {resource}"
                            ))
                        })?
                }
            };
            let mut relationships = Map::new();
            relationships.insert(
                parent.relationship.into(),
                json!({"data": {"type": parent.type_name, "id": id}}),
            );
            Some(Value::Object(relationships))
        }
    };

    let mut attributes = Map::new();
    for extra in ALL_EXTRAS {
        let value = str_arg(args, extra.arg);
        let allowed = kind.extras.iter().find(|e| e.arg == extra.arg);
        match (allowed, value) {
            (None, Some(_)) => {
                return Err(ToolError::new(format!(
                    "`{}` is not used for {resource}",
                    extra.arg
                )));
            }
            (Some(e), None) if e.required => {
                return Err(ToolError::new(format!(
                    "`{}` is required for {resource}: one of {}",
                    e.arg,
                    e.values.join(", ")
                )));
            }
            (Some(e), Some(v)) => {
                if !e.values.is_empty() && !e.values.contains(&v) {
                    return Err(ToolError::new(format!(
                        "`{}` must be one of {}",
                        e.arg,
                        e.values.join(", ")
                    )));
                }
                attributes.insert(e.attribute.into(), Value::String(v.to_owned()));
            }
            _ => {}
        }
    }
    Ok(Plan {
        kind,
        relationships,
        attributes,
    })
}

async fn upload_asset(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let Plan {
        kind,
        relationships,
        mut attributes,
    } = plan(&args)?;
    let wait = args.get("wait").and_then(Value::as_bool).unwrap_or(true);
    let timeout = timeout_arg(&args, DEFAULT_TIMEOUT, MAX_TIMEOUT)?;
    let file = upload::resolve_file(ctx, &args)?;
    let file_name = upload::file_name(str_arg(&args, "file_name"), &file)?;
    let file_size = upload::file_size(&file).await?;
    let checksum = upload::file_md5(&file).await?;

    // 1. Резервирование.
    attributes.insert("fileName".into(), json!(file_name));
    attributes.insert("fileSize".into(), json!(file_size));
    let mut data = json!({"type": kind.type_name, "attributes": attributes});
    if let Some(relationships) = relationships {
        data["relationships"] = relationships;
    }
    let created = ctx
        .api
        .execute(ApiRequest::new(HttpMethod::Post, kind.path).with_json(json!({"data": data})))
        .await
        .map_err(|e| ToolError::new(format!("could not reserve the {}: {e}", kind.resource)))?;
    let created = json_body(created, "the reservation")?;
    let id = data_id(&created, "the reservation")?;
    let instance = instance_path(kind.path, &id)?;
    let label = format!("{} {id}", kind.resource);

    // 2–3. Части и фиксация; при сбое — удаление резервирования.
    let empty = Value::Object(Map::new());
    let reserved = created.pointer("/data/attributes").unwrap_or(&empty);
    let committed =
        match send_and_commit(ctx, kind, &file, file_size, &checksum, reserved, &id).await {
            Ok(body) => body,
            Err(reason) => {
                let cleanup = if kind.deletable {
                    delete_reservation(ctx, &instance, &label).await
                } else {
                    format!(
                        "The unfinished {label} remains in App Store Connect: the API has no \
                     delete operation for {}.",
                        kind.resource
                    )
                };
                return Err(ToolError::new(format!(
                    "Uploading {file_name} as {} failed: {reason}. {cleanup}",
                    kind.resource
                )));
            }
        };

    let mut state = read_state(kind, &committed);
    let mut finished = state.finished();
    if wait && !finished {
        let polled = upload::poll(timeout, SCHEDULE, || async {
            let response = ctx
                .api
                .execute(ApiRequest::new(HttpMethod::Get, instance.as_str()))
                .await?;
            let body = json_body(response, "the asset")?;
            let state = read_state(kind, &body);
            let done = state.finished();
            Ok((state, done))
        })
        .await
        .map_err(|e| {
            ToolError::new(format!(
                "{file_name} was uploaded and committed as {label}, but checking its \
                 processing state failed: {e}. Check it later with {}.",
                tool_for(&kind.get_operation()).unwrap_or("api_request")
            ))
        })?;
        state = polled.last;
        finished = polled.finished;
    }

    if state.failed() {
        let errors = state.errors.join("; ");
        let delete = if kind.deletable {
            format!(
                " Delete it with {} (id {id}) and upload a corrected file.",
                tool_for(&kind.delete_operation()).unwrap_or("api_request")
            )
        } else {
            " Upload a corrected file.".to_owned()
        };
        return Err(ToolError::new(format!(
            "App Store Connect could not process {file_name} ({label}, state {}): {}.{delete}",
            state.state.as_deref().unwrap_or("FAILED"),
            if errors.is_empty() {
                "no details were given"
            } else {
                &errors
            }
        )));
    }

    let mut result = json!({
        "resource": kind.resource,
        "type": kind.type_name,
        "id": id,
        "fileName": file_name,
        "fileSize": file_size,
        "sourceFileChecksum": checksum,
        "state": state.state,
        "finished": finished,
    });
    if !state.warnings.is_empty() {
        result["warnings"] = json!(state.warnings);
    }
    if !state.errors.is_empty() {
        result["errors"] = json!(state.errors);
    }
    if let Some(video) = state.video {
        result["videoDeliveryState"] = video;
    }
    if !finished {
        let tool = tool_for(&kind.get_operation()).unwrap_or("api_request");
        result["hint"] = json!(if wait {
            format!(
                "Apple has not finished processing the file within the timeout. Check the \
                 state later with {tool} (id {id})."
            )
        } else {
            format!("Processing continues on Apple's side. Check the state with {tool} (id {id}).")
        });
    }
    Ok(result)
}

/// Части и фиксация. Ошибка — текст без адресов загрузки.
async fn send_and_commit(
    ctx: &Context,
    kind: &AssetKind,
    file: &std::path::Path,
    file_size: u64,
    checksum: &str,
    reserved: &Value,
    id: &str,
) -> Result<Value, String> {
    let parts = upload::parse_upload_operations(reserved, file_size).map_err(|e| e.message)?;
    let count = parts.len();
    upload::upload_parts(&ctx.external, file, parts).await?;

    let mut attributes = json!({"uploaded": true});
    match kind.checksum {
        Checksum::None => {}
        Checksum::Md5 => attributes["sourceFileChecksum"] = json!(checksum),
        Checksum::Checksums => {
            attributes["sourceFileChecksums"] =
                json!({"file": {"hash": checksum, "algorithm": "MD5"}});
        }
    }
    let path = instance_path(kind.path, id).map_err(|e| e.message)?;
    let body = json!({"data": {"type": kind.type_name, "id": id, "attributes": attributes}});
    let response = ctx
        .api
        .execute(ApiRequest::new(HttpMethod::Patch, path).with_json(body))
        .await
        .map_err(|e| format!("all {count} parts were sent, but the commit failed: {e}"))?;
    // Фиксация прошла; если тела нет, состояние узнаем опросом. Ошибкой это не считается:
    // иначе зафиксированный ресурс был бы удалён как незавершённый.
    Ok(json_body(response, "the commit").unwrap_or(Value::Null))
}

/// Состояние обработки из ответа.
struct AssetState {
    state: Option<String>,
    errors: Vec<String>,
    warnings: Vec<String>,
    video: Option<Value>,
}

impl AssetState {
    fn finished(&self) -> bool {
        !matches!(
            self.state.as_deref(),
            None | Some("AWAITING_UPLOAD" | "UPLOAD_COMPLETE")
        )
    }

    fn failed(&self) -> bool {
        self.state.as_deref() == Some("FAILED")
    }
}

fn read_state(kind: &AssetKind, body: &Value) -> AssetState {
    let attributes = body.pointer("/data/attributes");
    let field = |name: &str| attributes.and_then(|a| a.get(name));
    let video = field("videoDeliveryState")
        .filter(|v| !v.is_null())
        .cloned();
    match kind.state {
        StateField::Delivery => {
            let delivery = field("assetDeliveryState").cloned().unwrap_or(Value::Null);
            AssetState {
                state: delivery
                    .get("state")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                errors: state_messages(&delivery, "errors"),
                warnings: state_messages(&delivery, "warnings"),
                video,
            }
        }
        StateField::Plain => AssetState {
            state: field("state").and_then(Value::as_str).map(str::to_owned),
            errors: Vec::new(),
            warnings: Vec::new(),
            video,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use md5::{Digest, Md5};
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::composite::upload::test_support::{context, payload};

    fn spec() -> Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../appstore-connect-api/spec/openapi.oas.json"
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Схемы с `uploadOperations`, которых нет в таблице, и почему.
    const NOT_IN_TABLE: &[(&str, &str)] = &[
        ("BuildUploadFile", "uploaded by build_upload"),
        ("GameCenterAchievementImage", "deprecated /v1 operations"),
        ("GameCenterLeaderboardImage", "deprecated /v1 operations"),
        ("GameCenterLeaderboardSetImage", "deprecated /v1 operations"),
    ];

    fn ref_name(value: &Value) -> Option<&str> {
        value.get("$ref")?.as_str()?.rsplit('/').next()
    }

    /// Таблица совпадает со спецификацией: каждый ресурс с `uploadOperations` либо есть
    /// в таблице, либо исключён явно, а данные каждой строки взяты из спецификации.
    #[test]
    fn table_matches_the_spec() {
        let spec = spec();
        let schemas = spec["components"]["schemas"].as_object().unwrap();
        let paths = spec["paths"].as_object().unwrap();

        let uploadable: BTreeSet<&str> = schemas
            .iter()
            .filter(|(_, s)| {
                s.pointer("/properties/attributes/properties/uploadOperations")
                    .is_some()
            })
            .map(|(name, _)| name.as_str())
            .collect();
        let covered: BTreeSet<&str> = ASSETS
            .iter()
            .map(|k| k.schema)
            .chain(NOT_IN_TABLE.iter().map(|(s, _)| *s))
            .collect();
        assert_eq!(uploadable, covered, "upload-capable schemas vs the table");

        let mut resources = BTreeSet::new();
        for kind in ASSETS {
            assert!(
                resources.insert(kind.resource),
                "duplicate {}",
                kind.resource
            );
            let schema = &schemas[kind.schema];
            assert_eq!(
                schema["properties"]["type"]["enum"][0], kind.type_name,
                "{}",
                kind.resource
            );

            // Создание.
            let post = &paths[kind.path]["post"];
            assert_eq!(
                post["operationId"],
                format!("{}_createInstance", kind.resource)
            );
            let create = post
                .pointer("/requestBody/content/application~1json/schema")
                .and_then(ref_name)
                .unwrap();
            assert_eq!(create, format!("{}CreateRequest", kind.schema));
            let data = &schemas[create]["properties"]["data"];
            let attrs = data["properties"]["attributes"]["properties"]
                .as_object()
                .unwrap();
            let expected: BTreeSet<&str> = ["fileName", "fileSize"]
                .into_iter()
                .chain(kind.extras.iter().map(|e| e.attribute))
                .collect();
            let actual: BTreeSet<&str> = attrs.keys().map(String::as_str).collect();
            assert_eq!(actual, expected, "create attributes of {}", kind.resource);
            let required: BTreeSet<&str> = data["properties"]["attributes"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect();
            for extra in kind.extras {
                assert_eq!(
                    required.contains(extra.attribute),
                    extra.required,
                    "{} {}",
                    kind.resource,
                    extra.attribute
                );
                if !extra.values.is_empty() {
                    let prop = &attrs[extra.attribute];
                    let enum_values = match ref_name(prop) {
                        Some(name) => schemas[name]["enum"].clone(),
                        None => prop["enum"].clone(),
                    };
                    assert_eq!(enum_values, json!(extra.values), "{}", extra.attribute);
                }
            }
            let rels: BTreeMap<&str, &str> = data["properties"]["relationships"]["properties"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(name, rel)| {
                            (
                                name.as_str(),
                                rel["properties"]["data"]["properties"]["type"]["enum"][0]
                                    .as_str()
                                    .unwrap(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let parents: BTreeMap<&str, &str> = kind
                .parents
                .iter()
                .map(|p| (p.relationship, p.type_name))
                .collect();
            assert_eq!(rels, parents, "parents of {}", kind.resource);

            // Экземпляр: чтение, фиксация, удаление.
            let instance = &paths[&format!("{}/{{id}}", kind.path)];
            assert_eq!(instance["get"]["operationId"], kind.get_operation());
            assert_eq!(
                instance["patch"]["operationId"],
                format!("{}_updateInstance", kind.resource)
            );
            assert_eq!(
                instance.get("delete").is_some(),
                kind.deletable,
                "delete of {}",
                kind.resource
            );
            if kind.deletable {
                assert_eq!(instance["delete"]["operationId"], kind.delete_operation());
            }
            let update = &schemas[&format!("{}UpdateRequest", kind.schema)]["properties"]["data"]["properties"]
                ["attributes"]["properties"];
            assert!(update.get("uploaded").is_some(), "{}", kind.resource);
            let md5 = update
                .get("sourceFileChecksum")
                .is_some_and(|p| p.get("deprecated") != Some(&json!(true)));
            let checksums = update.get("sourceFileChecksums").is_some();
            let expected = match (md5, checksums) {
                (true, false) => Checksum::Md5,
                (false, true) => Checksum::Checksums,
                (false, false) => Checksum::None,
                (true, true) => panic!("both checksum attributes in {}", kind.resource),
            };
            assert_eq!(kind.checksum, expected, "checksum of {}", kind.resource);

            let attributes = &schema["properties"]["attributes"]["properties"];
            let state = if attributes.get("assetDeliveryState").is_some() {
                StateField::Delivery
            } else {
                assert!(attributes.get("state").is_some(), "{}", kind.resource);
                StateField::Plain
            };
            assert_eq!(kind.state, state, "state of {}", kind.resource);

            // Подсказки называют существующие инструменты.
            assert!(
                tool_for(&kind.get_operation()).is_some(),
                "{}",
                kind.resource
            );
            if kind.deletable {
                assert!(
                    tool_for(&kind.delete_operation()).is_some(),
                    "{}",
                    kind.resource
                );
            }
        }
        // Исключённые /v1 Game Center действительно устарели.
        for (schema, _) in NOT_IN_TABLE
            .iter()
            .filter(|(_, why)| why.contains("deprecated"))
        {
            let type_name = schemas[*schema]["properties"]["type"]["enum"][0]
                .as_str()
                .unwrap();
            assert_eq!(
                paths[&format!("/v1/{type_name}")]["post"]["deprecated"],
                true,
                "{schema}"
            );
        }
    }

    #[test]
    fn schema_is_closed_and_lists_resources() {
        let schema = input_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["resource"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            ASSETS.len()
        );
    }

    #[test]
    fn plan_checks_parents_and_extras() {
        let err = plan(&json!({"resource": "appScreenshots", "file": "a.png"})).unwrap_err();
        assert!(err.message.contains("parent_id is required"), "{err}");
        let err = plan(&json!({"resource": "appEventScreenshots", "parent_id": "1", "file": "a"}))
            .unwrap_err();
        assert!(err.message.contains("app_event_asset_type"), "{err}");
        let err = plan(&json!({"resource": "appScreenshots", "parent_id": "1", "mime_type": "x"}))
            .unwrap_err();
        assert!(err.message.contains("not used"), "{err}");
        let err =
            plan(&json!({"resource": "gameCenterActivityImages", "parent_id": "1"})).unwrap_err();
        assert!(
            err.message.contains("parent_relationship is required"),
            "{err}"
        );
        let ok = plan(&json!({
            "resource": "gameCenterActivityImages",
            "parent_id": "1",
            "parent_relationship": "version",
        }))
        .unwrap();
        assert_eq!(
            ok.relationships.unwrap(),
            json!({"version": {"data": {"type": "gameCenterActivityVersions", "id": "1"}}})
        );
        let err = plan(&json!({"resource": "appClipAdvancedExperienceImages", "parent_id": "1"}))
            .unwrap_err();
        assert!(err.message.contains("without a parent"), "{err}");
        let ok = plan(&json!({
            "resource": "appPreviews",
            "parent_id": "set-1",
            "preview_frame_time_code": "00:00:05:00",
            "mime_type": "video/mp4",
        }))
        .unwrap();
        assert_eq!(ok.attributes["previewFrameTimeCode"], "00:00:05:00");
        assert_eq!(ok.attributes["mimeType"], "video/mp4");
    }

    /// Резервирование с тремя частями на «хост загрузки» того же `wiremock`.
    fn reservation(server: &MockServer, size: usize, part: usize) -> Value {
        let mut ops = Vec::new();
        let mut offset = 0;
        let mut n = 1;
        while offset < size {
            let length = part.min(size - offset);
            ops.push(json!({
                "method": "PUT",
                "url": format!("{}/upload/part{n}?signature=SECRET-SIG", server.uri()),
                "offset": offset,
                "length": length,
                "requestHeaders": [
                    {"name": "Content-Type", "value": "image/png"},
                    {"name": "x-part", "value": n.to_string()},
                ],
            }));
            offset += length;
            n += 1;
        }
        json!({"data": {
            "type": "appScreenshots",
            "id": "shot-1",
            "attributes": {
                "fileName": "shot.png",
                "fileSize": size,
                "uploadOperations": ops,
                "assetDeliveryState": {"state": "AWAITING_UPLOAD"},
            },
        }})
    }

    fn delivery(state: &str, extra: Value) -> Value {
        let mut st = json!({"state": state});
        if let Value::Object(map) = extra {
            for (k, v) in map {
                st[k] = v;
            }
        }
        json!({"data": {"type": "appScreenshots", "id": "shot-1",
            "attributes": {"assetDeliveryState": st}}})
    }

    #[tokio::test]
    async fn uploads_screenshot_in_parts_and_waits() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let data = payload(2500);
        std::fs::write(dir.path().join("shot.png"), &data).unwrap();
        let ctx = context(&server.uri(), dir.path());

        Mock::given(method("POST"))
            .and(path("/v1/appScreenshots"))
            .and(body_partial_json(json!({"data": {
                "type": "appScreenshots",
                "attributes": {"fileName": "shot.png", "fileSize": 2500},
                "relationships": {"appScreenshotSet": {"data": {"type": "appScreenshotSets", "id": "set-1"}}},
            }})))
            .respond_with(ResponseTemplate::new(201).set_body_json(reservation(&server, 2500, 1000)))
            .expect(1)
            .mount(&server)
            .await;
        for n in 1..=3 {
            let start = (n - 1) * 1000;
            let end = (start + 1000).min(2500);
            Mock::given(method("PUT"))
                .and(path(format!("/upload/part{n}")))
                .and(header("content-type", "image/png"))
                .and(header("x-part", n.to_string().as_str()))
                .and(wiremock::matchers::body_bytes(data[start..end].to_vec()))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
        }
        let md5 = upload::hex(&Md5::digest(&data));
        Mock::given(method("PATCH"))
            .and(path("/v1/appScreenshots/shot-1"))
            .and(body_partial_json(json!({"data": {
                "type": "appScreenshots", "id": "shot-1",
                "attributes": {"uploaded": true, "sourceFileChecksum": md5},
            }})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(delivery("UPLOAD_COMPLETE", json!({}))),
            )
            .expect(1)
            .mount(&server)
            .await;
        // Первый опрос — ещё обрабатывается, второй — готово.
        Mock::given(method("GET"))
            .and(path("/v1/appScreenshots/shot-1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(delivery("UPLOAD_COMPLETE", json!({}))),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/appScreenshots/shot-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(delivery(
                "COMPLETE",
                json!({"warnings": [{"code": "IMAGE_TOO_SMALL", "description": "Scaled"}]}),
            )))
            .mount(&server)
            .await;

        let result = upload_asset(
            &ctx,
            json!({
                "resource": "appScreenshots",
                "parent_id": "set-1",
                "file": "shot.png",
            }),
        )
        .await
        .unwrap();
        assert_eq!(result["id"], "shot-1");
        assert_eq!(result["state"], "COMPLETE");
        assert_eq!(result["finished"], true);
        assert_eq!(result["fileSize"], 2500);
        assert_eq!(result["sourceFileChecksum"], md5);
        assert_eq!(result["warnings"], json!(["IMAGE_TOO_SMALL: Scaled"]));
        let text = result.to_string();
        assert!(!text.contains("/upload/"), "{text}");
        assert!(!text.contains("SECRET-SIG"), "{text}");
        assert!(!text.contains("uploadOperations"), "{text}");
    }

    #[tokio::test]
    async fn failed_part_deletes_reservation() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), payload(2500)).unwrap();
        let ctx = context(&server.uri(), dir.path());

        Mock::given(method("POST"))
            .and(path("/v1/appScreenshots"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(reservation(&server, 2500, 1000)),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/part2"))
            .respond_with(ResponseTemplate::new(403).set_body_string(format!(
                "denied {}/upload/part2?signature=SECRET-SIG",
                server.uri()
            )))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/appScreenshots/shot-1"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let err = upload_asset(
            &ctx,
            json!({"resource": "appScreenshots", "parent_id": "set-1", "file": "shot.png"}),
        )
        .await
        .unwrap_err();
        let text = err.message;
        assert!(text.contains("part 2 of 3"), "{text}");
        assert!(text.contains("bytes 1000-1999"), "{text}");
        assert!(text.contains("HTTP 403"), "{text}");
        assert!(text.contains("was deleted"), "{text}");
        assert!(!text.contains("/upload/"), "{text}");
        assert!(!text.contains("SECRET-SIG"), "{text}");
        assert!(!text.contains("127.0.0.1"), "{text}");
    }

    #[tokio::test]
    async fn failed_commit_reports_failed_cleanup() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), payload(10)).unwrap();
        let ctx = context(&server.uri(), dir.path());
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(reservation(&server, 10, 10)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({"errors": [
                {"status": "409", "code": "ENTITY_ERROR", "title": "Checksum mismatch"}
            ]})))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let err = upload_asset(
            &ctx,
            json!({"resource": "appScreenshots", "parent_id": "set-1", "file": "shot.png"}),
        )
        .await
        .unwrap_err();
        assert!(err.message.contains("commit failed"), "{err}");
        assert!(err.message.contains("Checksum mismatch"), "{err}");
        assert!(err.message.contains("could not be deleted"), "{err}");
    }

    #[tokio::test]
    async fn processing_failure_reports_apple_errors() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), payload(10)).unwrap();
        let ctx = context(&server.uri(), dir.path());
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(reservation(&server, 10, 10)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(delivery("UPLOAD_COMPLETE", json!({}))),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(delivery(
                "FAILED",
                json!({"errors": [{"code": "IMAGE_INCORRECT_DIMENSIONS",
                    "description": "The dimensions of one or more screenshots are wrong."}]}),
            )))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;
        let err = upload_asset(
            &ctx,
            json!({"resource": "appScreenshots", "parent_id": "set-1", "file": "shot.png"}),
        )
        .await
        .unwrap_err();
        let text = err.message;
        assert!(text.contains("IMAGE_INCORRECT_DIMENSIONS"), "{text}");
        assert!(text.contains("dimensions of one or more"), "{text}");
        assert!(text.contains("shot-1"), "{text}");
        assert!(!text.contains("SECRET-SIG"), "{text}");
    }

    #[tokio::test]
    async fn no_wait_returns_hint() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("event.png"), payload(10)).unwrap();
        let ctx = context(&server.uri(), dir.path());
        let mut reserved = reservation(&server, 10, 10);
        reserved["data"]["type"] = json!("appEventScreenshots");
        Mock::given(method("POST"))
            .and(path("/v1/appEventScreenshots"))
            .and(body_partial_json(
                json!({"data": {"attributes": {"appEventAssetType": "EVENT_CARD"}}}),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(reserved))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/appEventScreenshots/shot-1"))
            .respond_with(|req: &Request| {
                // У скриншотов событий нет атрибута контрольной суммы.
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                assert!(
                    body["data"]["attributes"]
                        .get("sourceFileChecksum")
                        .is_none()
                );
                ResponseTemplate::new(200).set_body_json(delivery("UPLOAD_COMPLETE", json!({})))
            })
            .mount(&server)
            .await;
        let result = upload_asset(
            &ctx,
            json!({
                "resource": "appEventScreenshots",
                "parent_id": "loc-1",
                "app_event_asset_type": "EVENT_CARD",
                "file": "event.png",
                "wait": false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(result["finished"], false);
        assert_eq!(result["state"], "UPLOAD_COMPLETE");
        let hint = result["hint"].as_str().unwrap();
        let tool = tool_for("appEventScreenshots_getInstance").unwrap();
        assert!(hint.contains(tool), "{hint}");
        assert!(!result.to_string().contains("SECRET-SIG"));
    }
}
