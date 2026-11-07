//! Инварианты таблиц операций, сгенерированных `build.rs`, и снапшоты характерных операций.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use appstore_connect_api::spec::{
    AccessLevel, BodyKind, ExclusionReason, HttpMethod, OperationDef, ParamLocation, ParamType,
    ResponseKind, excluded_operations, find, find_excluded, find_toolset, operations, toolsets,
};
use serde_json::{Value, json};

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

/// Наборы инструментов — ровно эти, в этом порядке.
const TOOLSETS: [&str; 21] = [
    "apps",
    "versions-review",
    "metadata",
    "media",
    "builds",
    "testflight",
    "xcode-cloud",
    "iap",
    "subscriptions",
    "pricing-availability",
    "reviews",
    "analytics-reports",
    "provisioning",
    "users",
    "webhooks",
    "sandbox",
    "game-center",
    "app-clips",
    "product-pages-events",
    "background-assets",
    "alt-distribution",
];

fn spec() -> &'static Value {
    static SPEC: OnceLock<Value> = OnceLock::new();
    SPEC.get_or_init(|| {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/spec/openapi.oas.json"
        ))
        .expect("снимок спецификации читается");
        serde_json::from_str(&raw).expect("спецификация — JSON")
    })
}

/// Операции спецификации по `operationId`: путь, метод, объект операции.
fn spec_operations() -> BTreeMap<String, (String, &'static str, &'static Value)> {
    let mut out = BTreeMap::new();
    for (path, item) in spec()["paths"].as_object().expect("paths") {
        for method in METHODS {
            if let Some(op) = item.get(method) {
                let id = op["operationId"].as_str().expect("operationId").to_owned();
                out.insert(id, (path.clone(), method, op));
            }
        }
    }
    out
}

fn schema(op: &OperationDef) -> Value {
    serde_json::from_str(op.input_schema)
        .unwrap_or_else(|err| panic!("{}: input_schema не JSON: {err}", op.tool_name))
}

fn op(name: &str) -> &'static OperationDef {
    find(name).unwrap_or_else(|| panic!("нет операции {name}"))
}

fn by_operation_id(id: &str) -> &'static OperationDef {
    operations()
        .iter()
        .find(|op| op.operation_id == id)
        .unwrap_or_else(|| panic!("операция {id} не зарегистрирована"))
}

#[test]
fn every_spec_operation_is_kept_or_excluded() {
    let expected = spec_operations();
    assert_eq!(expected.len(), 1337, "число операций спецификации 4.6");
    assert_eq!(
        operations().len() + excluded_operations().len(),
        expected.len()
    );

    let kept: BTreeSet<&str> = operations().iter().map(|op| op.operation_id).collect();
    let excluded: BTreeSet<&str> = excluded_operations()
        .iter()
        .map(|op| op.operation_id)
        .collect();
    assert!(kept.is_disjoint(&excluded));
    let all: BTreeSet<&str> = kept.union(&excluded).copied().collect();
    let in_spec: BTreeSet<&str> = expected.keys().map(String::as_str).collect();
    assert_eq!(all, in_spec);

    for op in operations() {
        let (path, method, raw) = &expected[op.operation_id];
        assert_eq!(op.path, path, "{}", op.tool_name);
        assert_eq!(
            op.method.as_str(),
            method.to_uppercase(),
            "{}",
            op.tool_name
        );
        assert!(!op.summary.is_empty(), "{}: нет summary", op.tool_name);
        assert_ne!(raw["deprecated"], true, "{}: устаревшая", op.tool_name);
        let tag = raw["tags"][0].as_str().expect("тег");
        let toolset = find_toolset(op.toolset)
            .unwrap_or_else(|| panic!("{}: нет набора {}", op.tool_name, op.toolset));
        assert!(
            toolset.tags.contains(&tag),
            "{}: набор не по тегу",
            op.tool_name
        );
    }
    for ex in excluded_operations() {
        let (path, method, raw) = &expected[ex.operation_id];
        assert_eq!(ex.path, path, "{}", ex.operation_id);
        assert_eq!(ex.method.as_str(), method.to_uppercase());
        assert!(!ex.summary.is_empty(), "{}", ex.operation_id);
        let tag = raw["tags"][0].as_str().expect("тег");
        assert!(
            find_toolset(ex.toolset).is_some_and(|t| t.tags.contains(&tag)),
            "{}",
            ex.operation_id
        );
        assert_eq!(
            find_excluded(ex.operation_id).map(|e| e.operation_id),
            Some(ex.operation_id)
        );
    }
    assert!(find_excluded("apps_getCollection").is_none());
}

#[test]
fn exclusion_reasons_follow_rules() {
    let expected = spec_operations();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for ex in excluded_operations() {
        *counts.entry(ex.reason.as_str()).or_default() += 1;
        let (path, method, raw) = &expected[ex.operation_id];
        match ex.reason {
            ExclusionReason::Deprecated => assert_eq!(raw["deprecated"], true),
            ExclusionReason::RelationshipLinkage => {
                assert_eq!(*method, "get");
                assert!(path.contains("/relationships/"), "{path}");
                assert_ne!(raw["deprecated"], true);
            }
            ExclusionReason::DuplicateList => {
                assert!(ex.operation_id.ends_with("_getToManyRelated"));
                assert!(
                    ex.alternative.starts_with("Call `list_"),
                    "{}: {}",
                    ex.operation_id,
                    ex.alternative
                );
            }
        }
        assert!(!ex.reason.explanation().is_empty());
    }
    assert_eq!(
        counts,
        BTreeMap::from([
            ("deprecated", 159),
            ("duplicate_list", 10),
            ("relationship_linkage", 265),
        ])
    );

    // Запись связей остаётся инструментами.
    let relationship_writes: Vec<&str> = expected
        .iter()
        .filter(|(_, (path, method, raw))| {
            *method != "get" && path.contains("/relationships/") && raw["deprecated"] != true
        })
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(relationship_writes.len(), 54);
    for id in relationship_writes {
        by_operation_id(id);
    }

    let dup = find_excluded("apps_builds_getToManyRelated").expect("дубликат");
    assert_eq!(
        dup.alternative,
        "Call `list_builds` with `filter.app` set to the parent ID."
    );
    let linkage = find_excluded("apps_appInfos_getToManyRelationship").expect("связь");
    assert!(linkage.alternative.contains("`list_app_infos`"));
    let deprecated = find_excluded("gameCenterLeaderboards_getInstance").expect("устаревшая");
    assert!(
        deprecated
            .alternative
            .contains("`get_game_center_leaderboard`"),
        "{}",
        deprecated.alternative
    );
}

#[test]
fn tool_names_are_unique_sorted_and_valid() {
    let names: Vec<&str> = operations().iter().map(|op| op.tool_name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(names, sorted, "имена должны быть уникальны и отсортированы");
    for name in names {
        assert!(
            !name.is_empty()
                && name.len() <= 40
                && name.starts_with(|c: char| c.is_ascii_lowercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "недопустимое имя инструмента {name}"
        );
        // Префикс клиента `mcp__appstore-connect__` + имя укладываются в 64 символа.
        assert!(format!("mcp__appstore-connect__{name}").len() <= 64);
        assert_eq!(find(name).map(|op| op.tool_name), Some(name));
    }
    assert!(find("no_such_tool").is_none());

    for (name, id) in [
        ("list_apps", "apps_getCollection"),
        ("get_app", "apps_getInstance"),
        (
            "create_app_store_version",
            "appStoreVersions_createInstance",
        ),
        ("list_builds", "builds_getCollection"),
        (
            "add_beta_group_builds",
            "betaGroups_builds_createToManyRelationship",
        ),
        (
            "remove_beta_group_builds",
            "betaGroups_builds_deleteToManyRelationship",
        ),
        (
            "set_app_store_version_build",
            "appStoreVersions_build_updateToOneRelationship",
        ),
        ("create_app_screenshot", "appScreenshots_createInstance"),
        (
            "list_app_store_versions",
            "apps_appStoreVersions_getToManyRelated",
        ),
        ("get_build_app", "builds_app_getToOneRelated"),
        (
            "create_in_app_purchase_image",
            "inAppPurchaseImages_createInstance",
        ),
        (
            "create_in_app_purchase_image_v2",
            "inAppPurchaseImagesV2_createInstance",
        ),
    ] {
        assert_eq!(op(name).operation_id, id, "{name}");
    }
}

#[test]
fn toolsets_cover_all_tags() {
    let slugs: Vec<&str> = toolsets().iter().map(|t| t.slug).collect();
    assert_eq!(slugs, TOOLSETS);

    let mut tags = BTreeSet::new();
    for toolset in toolsets() {
        assert!(!toolset.description.is_empty(), "{}", toolset.slug);
        assert!(
            toolset.description.is_ascii(),
            "{}: описание набора — для агента, на английском",
            toolset.slug
        );
        for tag in toolset.tags {
            assert!(tags.insert(*tag), "тег {tag} в двух наборах");
        }
        assert!(
            operations().iter().any(|op| op.toolset == toolset.slug),
            "пустой набор {}",
            toolset.slug
        );
        assert_eq!(
            find_toolset(toolset.slug).map(|t| t.slug),
            Some(toolset.slug)
        );
    }
    let spec_tags: BTreeSet<&str> = spec_operations()
        .values()
        .filter_map(|(_, _, raw)| raw["tags"][0].as_str())
        .collect();
    assert_eq!(spec_tags.len(), 207);
    assert_eq!(
        tags, spec_tags,
        "каждый тег спецификации — ровно в одном наборе"
    );
}

#[test]
fn access_levels() {
    for op in operations() {
        let relationship = op.path.contains("/relationships/");
        match op.method {
            HttpMethod::Get => assert_eq!(op.access, AccessLevel::Read, "{}", op.tool_name),
            HttpMethod::Delete if !relationship => {
                assert_eq!(op.access, AccessLevel::Destructive, "{}", op.tool_name)
            }
            _ => {}
        }
        if op.access != default_access(op) {
            // Исключение объяснено агенту в описании.
            assert!(
                op.description.starts_with("Destructive: ")
                    || op.description.starts_with("Read-only: ")
                    || op.description.starts_with("Write: "),
                "{}: {}",
                op.tool_name,
                op.description
            );
        }
    }

    // Удаление связи — запись.
    assert_eq!(op("remove_beta_group_builds").access, AccessLevel::Write);
    assert_eq!(op("remove_beta_group_testers").access, AccessLevel::Write);
    assert_eq!(op("add_beta_group_builds").access, AccessLevel::Write);
    assert_eq!(op("set_app_store_version_build").access, AccessLevel::Write);
    assert_eq!(op("create_app_store_version").access, AccessLevel::Write);
    assert_eq!(op("list_apps").access, AccessLevel::Read);
    assert_eq!(op("delete_app_screenshot").access, AccessLevel::Destructive);
    assert_eq!(op("delete_certificate").access, AccessLevel::Destructive);
    assert_eq!(
        op("create_gc_matchmaking_rule_set_test").access,
        AccessLevel::Read
    );

    for name in [
        // Отправка на ревью.
        "create_review_submission",
        "update_review_submission",
        "create_review_submission_item",
        "create_beta_app_review_submission",
        "create_in_app_purchase_submission",
        "create_subscription_submission",
        "create_subscription_group_submission",
        // Выпуск версии и поэтапный релиз.
        "create_app_store_version_release_request",
        "create_app_store_version_phased_release",
        "update_app_store_version_phased_release",
        // Цены и доступность.
        "create_app_price_schedule",
        "create_in_app_purchase_price_schedule",
        "create_subscription_price",
        "remove_subscription_prices",
        "create_app_availability",
        "update_territory_availability",
        "create_end_app_availability_pre_order",
        // Сертификаты, пользователи, тестировщики, отзывы.
        "update_certificate",
        "update_user",
        "create_user_invitation",
        "add_user_visible_apps",
        "remove_user_visible_apps",
        "create_beta_tester_invitation",
        "create_beta_tester",
        "add_beta_group_testers",
        "create_customer_review_response",
    ] {
        assert_eq!(op(name).access, AccessLevel::Destructive, "{name}");
    }
}

fn default_access(op: &OperationDef) -> AccessLevel {
    match op.method {
        HttpMethod::Get => AccessLevel::Read,
        HttpMethod::Delete if op.path.contains("/relationships/") => AccessLevel::Write,
        HttpMethod::Delete => AccessLevel::Destructive,
        _ => AccessLevel::Write,
    }
}

#[test]
fn input_schemas_are_strict_objects_with_all_params() {
    let mut total = 0;
    for op in operations() {
        total += op.input_schema.len();
        assert!(
            op.input_schema.len() < 16 * 1024,
            "{}: схема {} байт",
            op.tool_name,
            op.input_schema.len()
        );
        let schema = schema(op);
        assert_eq!(schema["type"], "object", "{}", op.tool_name);
        assert_eq!(schema["additionalProperties"], false, "{}", op.tool_name);
        assert!(!op.input_schema.contains("$ref"), "{}", op.tool_name);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{}: нет properties", op.tool_name));
        let param_names: BTreeSet<&str> = op.params.iter().map(|p| p.name).collect();
        let property_names: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
        assert_eq!(param_names, property_names, "{}", op.tool_name);
        assert_eq!(param_names.len(), op.params.len(), "{}", op.tool_name);
        assert!(
            !property_names
                .iter()
                .any(|name| name.eq_ignore_ascii_case("authorization")),
            "{}",
            op.tool_name
        );

        let required: BTreeSet<&str> = schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let expected: BTreeSet<&str> = op
            .params
            .iter()
            .filter(|p| p.required)
            .map(|p| p.name)
            .collect();
        assert_eq!(required, expected, "{}", op.tool_name);

        for param in op.params {
            assert!(
                !param.description.is_empty(),
                "{}: {}",
                op.tool_name,
                param.name
            );
            let property = &properties[param.name];
            assert_eq!(
                property["description"], param.description,
                "{}",
                op.tool_name
            );
            let kind = match param.value_type {
                ParamType::String => "string",
                ParamType::Integer => "integer",
                ParamType::Number => "number",
                ParamType::Boolean => "boolean",
                ParamType::Array => "array",
                ParamType::Object => "object",
            };
            assert_eq!(property["type"], kind, "{}: {}", op.tool_name, param.name);
        }
        check_schema(op.tool_name, "", &schema, false);
    }
    // Все схемы вместе: ориентир для контекста агента, сейчас около 0,9 МБ.
    assert!(total < 1_100_000, "схемы выросли до {total} байт");
}

/// Схема понятна MCP-клиентам: известные типы, `items` у массивов, имена свойств по шаблону
/// `^[a-zA-Z0-9_.-]{1,64}$` (ограничение API Claude), непустой `required` из имён свойств.
/// Варианты `anyOf`/`oneOf`/`allOf` (`variant`) у Apple — ограничения без `properties`:
/// их `required` ссылается на свойства родительской схемы.
fn check_schema(tool: &str, at: &str, schema: &Value, variant: bool) {
    const TYPES: [&str; 7] = [
        "string", "number", "integer", "boolean", "array", "object", "null",
    ];
    let object = schema
        .as_object()
        .unwrap_or_else(|| panic!("{tool}{at}: схема не объект"));
    for key in object.keys() {
        assert!(
            !key.starts_with("x-")
                && !matches!(
                    key.as_str(),
                    "example" | "examples" | "$ref" | "nullable" | "discriminator" | "title"
                ),
            "{tool}{at}: лишний ключ {key}"
        );
    }
    let kinds: Vec<&str> = match object.get("type") {
        None => Vec::new(),
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        Some(other) => panic!("{tool}{at}: тип {other}"),
    };
    for kind in &kinds {
        assert!(TYPES.contains(kind), "{tool}{at}: тип {kind}");
    }
    assert!(
        !kinds.contains(&"array") || object.contains_key("items"),
        "{tool}{at}: массив без items"
    );
    let properties = object.get("properties").and_then(Value::as_object);
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .unwrap_or_else(|| panic!("{tool}{at}: required не массив"));
        assert!(!required.is_empty(), "{tool}{at}: пустой required");
        for name in required {
            let name = name.as_str().expect("имя в required");
            assert!(
                variant || properties.is_some_and(|p| p.contains_key(name)),
                "{tool}{at}: required {name} нет в properties"
            );
        }
    }
    for (name, property) in properties.into_iter().flatten() {
        assert!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
            "{tool}{at}: недопустимое имя свойства {name}"
        );
        check_schema(tool, &format!("{at}.{name}"), property, variant);
    }
    if let Some(items) = object.get("items") {
        check_schema(tool, &format!("{at}[]"), items, variant);
    }
    if let Some(additional) = object.get("additionalProperties")
        && additional.is_object()
    {
        check_schema(tool, &format!("{at}{{}}"), additional, variant);
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(variants) = object.get(key) {
            for variant in variants.as_array().expect("варианты") {
                check_schema(tool, &format!("{at}|"), variant, true);
            }
        }
    }
}

#[test]
fn params_match_spec_names_and_templates() {
    for op in operations() {
        let placeholders: BTreeSet<&str> = op
            .path
            .split('{')
            .skip(1)
            .filter_map(|part| part.split_once('}').map(|(name, _)| name))
            .collect();
        let declared: BTreeSet<&str> = op
            .params_in(ParamLocation::Path)
            .map(|p| p.api_name)
            .collect();
        assert_eq!(placeholders, declared, "{}", op.tool_name);
        assert!(op.params_in(ParamLocation::Path).all(|p| p.required));
        assert!(
            ["/v1/", "/v2/", "/v3/"]
                .iter()
                .any(|prefix| op.path.starts_with(prefix)),
            "{}: {}",
            op.tool_name,
            op.path
        );
        for param in op.params {
            // Аргумент `filter.app` ↔ параметр API `filter[app]`.
            let expected = match param.api_name.split_once('[') {
                Some((head, rest)) => format!("{head}.{}", rest.trim_end_matches(']')),
                None => param.api_name.to_owned(),
            };
            assert_eq!(param.name, expected, "{}", op.tool_name);
            assert_eq!(
                op.param(param.name).map(|p| p.api_name),
                Some(param.api_name)
            );
        }

        let bodies: Vec<_> = op.params_in(ParamLocation::Body).collect();
        match op.body {
            None => assert!(bodies.is_empty(), "{}", op.tool_name),
            Some(BodyKind::Json) => assert!(
                matches!(bodies.as_slice(), [body] if body.name == "body" && body.value_type == ParamType::Object),
                "{}",
                op.tool_name
            ),
        }
        if op.method == HttpMethod::Post || op.method == HttpMethod::Patch {
            assert!(op.body.is_some(), "{}: запись без тела", op.tool_name);
        }
    }
}

#[test]
fn response_kinds_and_accept() {
    let binary: BTreeMap<&str, &str> = operations()
        .iter()
        .filter(|op| op.response == ResponseKind::Binary)
        .map(|op| (op.tool_name, op.accept))
        .collect();
    assert_eq!(
        binary,
        BTreeMap::from([
            ("get_iap_offer_code_one_time_code_values", "text/csv"),
            ("get_sub_offer_code_one_time_code_values", "text/csv"),
            ("list_finance_reports", "application/a-gzip"),
            ("list_sales_reports", "application/a-gzip"),
        ])
    );
    let vendor: BTreeMap<&str, &str> = operations()
        .iter()
        .filter(|op| op.response == ResponseKind::Json && op.accept != "application/json")
        .map(|op| (op.tool_name, op.accept))
        .collect();
    assert_eq!(
        vendor,
        BTreeMap::from([
            (
                "list_app_perf_power_metrics",
                "application/vnd.apple.xcode-metrics+json"
            ),
            (
                "list_app_performance_overviews",
                "application/vnd.apple.xcode-overview+json"
            ),
            (
                "list_build_perf_power_metrics",
                "application/vnd.apple.xcode-metrics+json"
            ),
            (
                "list_diagnostic_signature_logs",
                "application/vnd.apple.diagnostic-logs+json"
            ),
        ])
    );
}

#[test]
fn descriptions_hint_pagination_and_include() {
    let list = op("list_apps");
    assert!(list.description.contains("`links.next`"));
    assert!(list.description.contains("`include`"));
    assert!(
        op("list_app_store_versions")
            .description
            .contains("`limit`")
    );
    assert!(!op("get_build_app").description.contains("links.next"));
    assert!(
        op("create_customer_review_response")
            .description
            .starts_with("Destructive: ")
    );
}

/// Снапшот операции. Ключи сортируются, чтобы порядок не зависел от фичи
/// `preserve_order` у `serde_json`, которую может включить любая зависимость.
macro_rules! snapshot {
    ($name:expr) => {
        insta::with_settings!({ sort_maps => true }, {
            insta::assert_json_snapshot!(snapshot_view(op($name)));
        })
    };
}

/// Представление операции для снапшота: все поля плюс разобранная JSON Schema.
fn snapshot_view(op: &OperationDef) -> Value {
    let params: Vec<Value> = op
        .params
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "api_name": p.api_name,
                "location": format!("{:?}", p.location),
                "required": p.required,
                "value_type": format!("{:?}", p.value_type),
                "description": p.description,
            })
        })
        .collect();
    json!({
        "tool_name": op.tool_name,
        "operation_id": op.operation_id,
        "toolset": op.toolset,
        "summary": op.summary,
        "description": op.description,
        "method": op.method.as_str(),
        "path": op.path,
        "access": format!("{:?}", op.access),
        "params": params,
        "body": op.body.map(|kind| format!("{kind:?}")),
        "response": format!("{:?}", op.response),
        "accept": op.accept,
        "input_schema": schema(op),
    })
}

#[test]
fn snapshot_list_apps() {
    // Список верхнего уровня: фильтры, сортировка, include, fields, limit.
    snapshot!("list_apps");
}

#[test]
fn snapshot_create_app_store_version() {
    // JSON:API-тело с атрибутами и связями, nullable-поля.
    snapshot!("create_app_store_version");
}

#[test]
fn snapshot_add_beta_group_builds() {
    // Запись связи: тело — идентификаторы ресурсов.
    snapshot!("add_beta_group_builds");
}

#[test]
fn snapshot_create_app_screenshot() {
    // Резервирование загрузки скриншота.
    snapshot!("create_app_screenshot");
}

#[test]
fn snapshot_list_sales_reports() {
    // Бинарный ответ (gzip) и обязательные фильтры.
    snapshot!("list_sales_reports");
}
