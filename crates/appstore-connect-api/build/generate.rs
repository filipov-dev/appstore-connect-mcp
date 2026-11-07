//! Построение таблиц операций из спецификации и файла-дополнения и генерация Rust-кода.
//!
//! Этапы: наборы инструментов → разбор операций (параметры, тело, ответ, уровень доступа,
//! причина исключения) → имена инструментов → описания → проверки → вывод `TOOLSETS`,
//! `OPERATIONS` и `EXCLUDED` и отчёта.
//!
//! Ошибки, которые исправляются правкой дополнения (тег без набора, длинное или повторяющееся
//! имя, устаревшая ссылка на `operationId`), собираются в список и выводятся разом: так
//! скрипт обновления спецификации показывает все проблемы новой версии за один прогон.
//! Ошибки формы спецификации (неизвестный тип параметра или ответа) роняют сборку сразу.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use crate::access::Exceptions;
use crate::naming::{Action, MAX_TOOL_NAME, OperationName, abbreviate, is_valid_tool_name};
use crate::overlay::{Level, Overlay};
use crate::schema::{Resolver, deref, description_of};

const METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// Ключи элемента `paths`, кроме методов, которые генератор понимает.
const PATH_ITEM_KEYS: &[&str] = &["parameters", "summary", "description"];

const JSON_MEDIA: &str = "application/json";

/// Тела ответов, которые агент получает как файл (`save_to`), а не как JSON.
const BINARY_MEDIA: &[&str] = &["application/a-gzip", "application/octet-stream", "text/csv"];

/// Типы ответа, похожие на JSON (`application/vnd.apple.xcode-metrics+json`).
fn is_json_media(media: &str) -> bool {
    media == JSON_MEDIA || media.ends_with("+json")
}

/// Оставлять ли `enum` у `fields[<тип>]` включаемых (`include`) ресурсов. Перечни полей
/// для каждого включаемого типа — большая часть размера схем списков (у `list_apps` —
/// две трети), а нужны они редко: агент видит поля в ответе. Значения проверяет API.
/// `enum` у `fields[<тип>]` самого ресурса операции остаётся.
const INCLUDED_FIELDS_ENUM: bool = false;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Path,
    Query,
    Body,
}

impl Location {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Path => "ParamLocation::Path",
            Self::Query => "ParamLocation::Query",
            Self::Body => "ParamLocation::Body",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseKind {
    Json,
    Binary,
}

impl ResponseKind {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Json => "ResponseKind::Json",
            Self::Binary => "ResponseKind::Binary",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Exclusion {
    Deprecated,
    RelationshipLinkage,
    DuplicateList,
}

impl Exclusion {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Deprecated => "ExclusionReason::Deprecated",
            Self::RelationshipLinkage => "ExclusionReason::RelationshipLinkage",
            Self::DuplicateList => "ExclusionReason::DuplicateList",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Deprecated => "deprecated",
            Self::RelationshipLinkage => "relationship_linkage",
            Self::DuplicateList => "duplicate_list",
        }
    }
}

#[derive(Debug, Clone)]
struct Param {
    /// Ключ в аргументах инструмента: `filter[app]` → `filter.app`.
    name: String,
    /// Имя в пути или строке запроса, как в спецификации.
    api_name: String,
    location: Location,
    required: bool,
    description: String,
    /// Имя варианта `ParamType` в `spec.rs`.
    value_type: &'static str,
    /// JSON Schema значения без описания.
    schema: Value,
}

#[derive(Debug)]
struct Operation {
    operation_id: String,
    name: Option<OperationName>,
    tool_name: String,
    toolset: String,
    summary: String,
    description: String,
    method: &'static str,
    path: String,
    access: Level,
    access_reason: Option<String>,
    params: Vec<Param>,
    has_body: bool,
    response: ResponseKind,
    accept: String,
    exclusion: Option<Exclusion>,
    alternative: String,
    input_schema: String,
}

pub struct Output {
    /// Код `operations.rs`; пустой, если есть ошибки.
    pub code: String,
    /// Отчёт (JSON); пишется и при ошибках, чтобы скрипт обновления показал имена.
    pub report: String,
    /// Ошибки согласования спецификации и дополнения.
    pub errors: Vec<String>,
}

/// Генерирует исходный код `operations.rs` и отчёт из спецификации и дополнения.
pub fn generate(spec: &Value, overlay: &Overlay) -> Output {
    let mut errors = Vec::new();
    check_toolsets(spec, overlay, &mut errors);
    let exceptions = Exceptions::new(&overlay.access);
    let mut resolver = Resolver::new(spec);
    let mut operations = parse_operations(spec, overlay, &exceptions, &mut resolver, &mut errors);
    check_overlay_references(overlay, &exceptions, &operations, &mut errors);
    assign_names(overlay, &mut operations, &mut errors);
    resolve_alternatives(overlay, &mut operations, &mut errors);
    for op in operations.iter_mut().filter(|op| op.exclusion.is_none()) {
        op.description = describe(op);
        op.input_schema = input_schema(&op.operation_id, &op.params);
    }
    check_invariants(overlay, &operations, &mut errors);

    let (mut kept, mut excluded): (Vec<Operation>, Vec<Operation>) = operations
        .into_iter()
        .partition(|op| op.exclusion.is_none());
    kept.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
    excluded.sort_by(|a, b| a.operation_id.cmp(&b.operation_id));
    let code = if errors.is_empty() {
        emit(overlay, &kept, &excluded)
    } else {
        String::new()
    };
    Output {
        code,
        report: report(overlay, &kept, &excluded, &errors),
        errors,
    }
}

/// Наборы: корректные уникальные slug, каждый тег спецификации ровно в одном наборе,
/// в дополнении нет тегов, которых нет в спецификации.
fn check_toolsets(spec: &Value, overlay: &Overlay, errors: &mut Vec<String>) {
    let mut slugs = BTreeSet::new();
    let mut tags = BTreeMap::new();
    for toolset in &overlay.toolsets {
        if !is_valid_slug(&toolset.slug) {
            errors.push(format!("недопустимый slug набора {}", toolset.slug));
        }
        if !slugs.insert(toolset.slug.as_str()) {
            errors.push(format!("slug набора {} повторяется", toolset.slug));
        }
        if toolset.description.trim().is_empty() {
            errors.push(format!("у набора {} нет описания", toolset.slug));
        }
        for tag in &toolset.tags {
            if let Some(other) = tags.insert(tag.as_str(), toolset.slug.as_str()) {
                errors.push(format!(
                    "тег {tag} указан в наборах {other} и {}",
                    toolset.slug
                ));
            }
        }
    }
    let used = spec_tags(spec);
    for tag in &used {
        if !tags.contains_key(tag.as_str()) {
            errors.push(format!(
                "тег спецификации {tag} не входит ни в один набор: добавь его в [[toolsets]] spec/overlay.toml"
            ));
        }
    }
    for tag in tags.keys() {
        if !used.contains(*tag) {
            errors.push(format!(
                "тег {tag} из spec/overlay.toml не встречается в спецификации"
            ));
        }
    }
}

/// Первые теги операций спецификации (тег `Metrics` у метрик идёт вторым и не учитывается).
fn spec_tags(spec: &Value) -> BTreeSet<String> {
    let mut tags = BTreeSet::new();
    for item in paths(spec).values() {
        for method in METHODS {
            if let Some(tag) = item
                .get(*method)
                .and_then(|op| op.get("tags"))
                .and_then(|tags| tags.get(0))
                .and_then(Value::as_str)
            {
                tags.insert(tag.to_owned());
            }
        }
    }
    tags
}

fn paths(spec: &Value) -> &Map<String, Value> {
    spec.get("paths")
        .and_then(Value::as_object)
        .expect("в спецификации нет paths")
}

fn is_valid_slug(slug: &str) -> bool {
    slug.starts_with(|c: char| c.is_ascii_lowercase())
        && !slug.ends_with('-')
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn toolset_for<'o>(overlay: &'o Overlay, tag: &str) -> Option<&'o str> {
    overlay
        .toolsets
        .iter()
        .find(|toolset| toolset.tags.iter().any(|t| t == tag))
        .map(|toolset| toolset.slug.as_str())
}

fn parse_operations(
    spec: &Value,
    overlay: &Overlay,
    exceptions: &Exceptions<'_>,
    resolver: &mut Resolver<'_>,
    errors: &mut Vec<String>,
) -> Vec<Operation> {
    let duplicates: BTreeSet<&str> = overlay
        .exclude
        .duplicates
        .iter()
        .map(|d| d.operation.as_str())
        .collect();
    let mut operations = Vec::new();
    for (path, item) in paths(spec) {
        let item = item
            .as_object()
            .expect("элемент paths не является объектом");
        for key in item.keys() {
            assert!(
                METHODS.contains(&key.as_str()) || PATH_ITEM_KEYS.contains(&key.as_str()),
                "неподдерживаемый ключ {key} в пути {path}"
            );
        }
        let shared: Vec<&Value> = item
            .get("parameters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect();
        for method in METHODS {
            let Some(raw) = item.get(*method) else {
                continue;
            };
            let op = parse_operation(
                spec, overlay, exceptions, resolver, path, method, raw, &shared,
            );
            let deprecated = raw.get("deprecated").and_then(Value::as_bool) == Some(true);
            let linkage_read = *method == "get" && path.contains("/relationships/");
            let mut op = op;
            op.exclusion =
                if deprecated && !overlay.exclude.keep_deprecated.contains(&op.operation_id) {
                    Some(Exclusion::Deprecated)
                } else if linkage_read
                    && !overlay
                        .exclude
                        .keep_relationship_reads
                        .contains(&op.operation_id)
                {
                    Some(Exclusion::RelationshipLinkage)
                } else if duplicates.contains(op.operation_id.as_str()) {
                    Some(Exclusion::DuplicateList)
                } else {
                    None
                };
            if op.toolset.is_empty() {
                // Тег без набора уже попал в ошибки check_toolsets.
                continue;
            }
            if op.name.is_none()
                && op.exclusion.is_none()
                && !overlay.naming.overrides.contains_key(&op.operation_id)
            {
                errors.push(format!(
                    "{}: operationId не следует шаблону Apple, задай имя в [naming.overrides]",
                    op.operation_id
                ));
            }
            operations.push(op);
        }
    }
    operations
}

#[allow(clippy::too_many_arguments)]
fn parse_operation(
    spec: &Value,
    overlay: &Overlay,
    exceptions: &Exceptions<'_>,
    resolver: &mut Resolver<'_>,
    path: &str,
    method: &'static str,
    op: &Value,
    shared_params: &[&Value],
) -> Operation {
    let operation_id =
        str_field(op, "operationId").unwrap_or_else(|| panic!("у {method} {path} нет operationId"));
    let tag = op
        .get("tags")
        .and_then(|tags| tags.get(0))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("у {operation_id} нет тега"));
    let toolset = toolset_for(overlay, tag).unwrap_or_default().to_owned();
    let (access, access_reason) = exceptions.access_for(&operation_id, method, path);

    let own_params = op
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let primary_type = primary_type(spec, op);
    let mut params = Vec::new();
    for raw in shared_params.iter().copied().chain(own_params) {
        if let Some(param) = parse_param(spec, &operation_id, path, raw, primary_type.as_deref()) {
            params.push(param);
        }
    }
    let has_body = match op.get("requestBody") {
        Some(raw) => {
            params.push(parse_body(spec, resolver, &operation_id, raw));
            true
        }
        None => false,
    };
    let (response, accept) = response_kind(spec, &operation_id, op);
    check_path_params(&operation_id, path, &params);

    let name = OperationName::parse(&operation_id);
    let summary = name
        .as_ref()
        .map(OperationName::summary)
        .unwrap_or_default();
    Operation {
        operation_id,
        name,
        tool_name: String::new(),
        toolset,
        summary,
        description: String::new(),
        method,
        path: path.to_owned(),
        access,
        access_reason: access_reason.map(str::to_owned),
        params,
        has_body,
        response,
        accept,
        exclusion: None,
        alternative: String::new(),
        input_schema: String::new(),
    }
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_owned())
}

/// Тип ресурса, который возвращает операция (`data[].type` ответа), например `apps`.
fn primary_type(spec: &Value, op: &Value) -> Option<String> {
    let responses = op.get("responses")?.as_object()?;
    for (code, response) in responses {
        if !code.starts_with('2') {
            continue;
        }
        let schema = deref(spec, response)
            .get("content")?
            .get(JSON_MEDIA)?
            .get("schema")?;
        let data = deref(spec, deref(spec, schema).get("properties")?.get("data")?);
        let data = match data.get("items") {
            Some(items) => deref(spec, items),
            None => data,
        };
        return data
            .get("properties")?
            .get("type")?
            .get("enum")?
            .get(0)?
            .as_str()
            .map(str::to_owned);
    }
    None
}

/// Ключ аргумента для параметра JSON:API: `filter[app]` → `filter.app`,
/// `fields[apps]` → `fields.apps`. API Claude принимает в именах свойств схемы только
/// `[a-zA-Z0-9_.-]`, поэтому скобки заменяются точкой; исходное имя хранится в `api_name`.
fn argument_name(api_name: &str) -> String {
    match api_name.split_once('[') {
        Some((head, rest)) => {
            let inner = rest
                .strip_suffix(']')
                .unwrap_or_else(|| panic!("неожиданное имя параметра {api_name}"));
            format!("{head}.{inner}")
        }
        None => api_name.to_owned(),
    }
}

/// Параметр пути или запроса; `None` для устаревших параметров запроса.
fn parse_param(
    spec: &Value,
    operation_id: &str,
    path: &str,
    raw: &Value,
    primary_type: Option<&str>,
) -> Option<Param> {
    let raw = deref(spec, raw);
    let object = raw.as_object().expect("параметр не является объектом");
    let api_name = str_field(raw, "name").expect("у параметра нет имени");
    let location = match str_field(raw, "in").as_deref() {
        Some("path") => Location::Path,
        Some("query") => Location::Query,
        other => panic!("{operation_id}: неподдерживаемое расположение параметра {other:?}"),
    };
    if location == Location::Query && raw.get("deprecated").and_then(Value::as_bool) == Some(true) {
        // Устаревший параметр не попадает в схему; передать его можно через `api_request`.
        return None;
    }
    if let Some(style) = str_field(raw, "style") {
        let expected = if location == Location::Path {
            "simple"
        } else {
            "form"
        };
        assert_eq!(
            style, expected,
            "{operation_id}: неподдерживаемый style параметра {api_name}"
        );
    }
    let required =
        location == Location::Path || raw.get("required").and_then(Value::as_bool) == Some(true);
    let schema = raw
        .get("schema")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{operation_id}: у параметра {api_name} нет schema"));
    let (mut schema, value_type) = simple_schema(operation_id, &api_name, schema);
    if value_type == "Array" {
        // Массивы в строке запроса JSON:API — через запятую (`style: form`, `explode: false`):
        // так их и склеивает исполнитель.
        assert_ne!(
            raw.get("explode").and_then(Value::as_bool),
            Some(true),
            "{operation_id}: массив {api_name} с explode: true не поддерживается"
        );
    }
    if !INCLUDED_FIELDS_ENUM
        && let Some(kind) = api_name
            .strip_prefix("fields[")
            .and_then(|rest| rest.strip_suffix(']'))
        && Some(kind) != primary_type
        && let Some(items) = schema.get_mut("items").and_then(Value::as_object_mut)
    {
        items.remove("enum");
    }
    let description =
        compact_description(path, &api_name, &description_of(object).unwrap_or_default());
    Some(Param {
        name: argument_name(&api_name),
        api_name,
        location,
        required,
        description,
        value_type,
        schema,
    })
}

/// Короткое описание параметра вместо типовых фраз спецификации: у списков десятки
/// параметров `fields[…]` и `limit[…]`, и описания — заметная часть размера схемы.
/// Нетиповые описания остаются как есть.
fn compact_description(path: &str, api_name: &str, text: &str) -> String {
    let bracket = api_name
        .split_once('[')
        .and_then(|(_, rest)| rest.strip_suffix(']'));
    match (text, bracket) {
        ("the id of the requested resource", _) => {
            // `/v1/apps/{id}/builds` → `apps`: идентификатор — у ресурса перед `{id}`.
            let resource = path
                .split('/')
                .take_while(|segment| !segment.starts_with('{'))
                .last()
                .unwrap_or_default();
            format!("ID of the `{resource}` resource.")
        }
        ("maximum resources per page", _) => "Page size.".to_owned(),
        ("comma-separated list of relationships to include", _) => {
            "Related resources to add to `included`.".to_owned()
        }
        (text, _) if text.starts_with("comma-separated list of sort expressions") => {
            "Sort keys; prefix `-` for descending order.".to_owned()
        }
        (text, Some(kind)) if text.starts_with("the fields to include for returned resources") => {
            format!("Attributes to return for `{kind}`.")
        }
        (text, Some(kind)) if text.starts_with("maximum number of related") => {
            format!("Max number of included `{kind}`.")
        }
        (text, _) => text.to_owned(),
    }
}

/// JSON-тело (`requestBody`) — параметр `body`.
fn parse_body(spec: &Value, resolver: &mut Resolver<'_>, operation_id: &str, raw: &Value) -> Param {
    let raw = deref(spec, raw);
    let body_required = raw.get("required").and_then(Value::as_bool) == Some(true);
    let content = raw
        .get("content")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{operation_id}: requestBody без content"));
    let [(media_type, media)] = content.iter().collect::<Vec<_>>()[..] else {
        panic!("{operation_id}: у requestBody должен быть ровно один тип содержимого");
    };
    assert_eq!(
        media_type, JSON_MEDIA,
        "{operation_id}: неподдерживаемый тип requestBody {media_type}"
    );
    let schema = media
        .get("schema")
        .unwrap_or_else(|| panic!("{operation_id}: requestBody без schema"));
    let schema = resolver.resolve(schema);
    let own = description_of(raw.as_object().expect("requestBody не объект"))
        .map(|s| format!(" {s}."))
        .unwrap_or_default();
    Param {
        name: "body".to_owned(),
        api_name: "body".to_owned(),
        location: Location::Body,
        required: body_required,
        description: format!("JSON:API request body.{own}"),
        value_type: "Object",
        schema,
    }
}

/// Вид успешного ответа и заголовок `Accept`.
///
/// JSON — если хоть один ответ `2xx` отдаёт `application/json`; иначе тип ответа: JSON
/// Apple (`…+json`, метрики Xcode) или файл (gzip-отчёты продаж и финансов, CSV).
fn response_kind(spec: &Value, operation_id: &str, op: &Value) -> (ResponseKind, String) {
    let responses = op
        .get("responses")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{operation_id}: нет responses"));
    let mut media: BTreeSet<String> = BTreeSet::new();
    for (code, response) in responses {
        if !code.starts_with('2') {
            continue;
        }
        let response = deref(spec, response);
        media.extend(
            response
                .get("content")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(Map::keys)
                .cloned(),
        );
    }
    if media.is_empty() || media.contains(JSON_MEDIA) {
        return (ResponseKind::Json, JSON_MEDIA.to_owned());
    }
    let [only] = media.iter().collect::<Vec<_>>()[..] else {
        panic!("{operation_id}: несколько типов ответа без application/json: {media:?}");
    };
    if is_json_media(only) {
        (ResponseKind::Json, only.clone())
    } else if BINARY_MEDIA.contains(&only.as_str()) {
        (ResponseKind::Binary, only.clone())
    } else {
        panic!("{operation_id}: неподдерживаемый тип ответа {only}")
    }
}

/// Схема и тип простого параметра (path, query).
fn simple_schema(
    operation_id: &str,
    name: &str,
    object: &Map<String, Value>,
) -> (Value, &'static str) {
    let kind = match object.get("type") {
        None => "string",
        Some(Value::String(kind)) => kind.as_str(),
        Some(other) => panic!("{operation_id}: неподдерживаемый тип {other} параметра {name}"),
    };
    let value_type = match kind {
        "string" => "String",
        "integer" => "Integer",
        "number" => "Number",
        "boolean" => "Boolean",
        "array" => "Array",
        other => panic!("{operation_id}: неподдерживаемый тип {other} параметра {name}"),
    };
    let mut schema = Map::new();
    schema.insert("type".into(), json!(kind));
    for key in ["enum", "default", "minimum", "maximum"] {
        if let Some(value) = object.get(key) {
            schema.insert(key.into(), value.clone());
        }
    }
    if let Some(items) = object.get("items").and_then(Value::as_object) {
        let (items, item_type) = simple_schema(operation_id, name, items);
        assert_ne!(
            item_type, "Array",
            "{operation_id}: вложенный массив в параметре {name}"
        );
        schema.insert("items".into(), items);
    } else if value_type == "Array" {
        schema.insert("items".into(), json!({ "type": "string" }));
    }
    (Value::Object(schema), value_type)
}

/// Проверяет, что плейсхолдеры `{name}` пути совпадают с path-параметрами.
fn check_path_params(operation_id: &str, path: &str, params: &[Param]) {
    let placeholders: BTreeSet<&str> = path
        .split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}').map(|(name, _)| name))
        .collect();
    let declared: BTreeSet<&str> = params
        .iter()
        .filter(|p| p.location == Location::Path)
        .map(|p| p.api_name.as_str())
        .collect();
    assert_eq!(
        placeholders, declared,
        "{operation_id}: плейсхолдеры пути {path} не совпадают с path-параметрами"
    );
}

/// Все `operationId`, упомянутые в дополнении, есть в спецификации и упомянуты по делу.
fn check_overlay_references(
    overlay: &Overlay,
    exceptions: &Exceptions<'_>,
    operations: &[Operation],
    errors: &mut Vec<String>,
) {
    let by_id: BTreeMap<&str, &Operation> = operations
        .iter()
        .map(|op| (op.operation_id.as_str(), op))
        .collect();
    let check = |errors: &mut Vec<String>, section: &str, id: &str| {
        if !by_id.contains_key(id) {
            errors.push(format!(
                "{section}: операции {id} нет в спецификации; убери её из spec/overlay.toml"
            ));
        }
    };
    for id in exceptions.operations() {
        check(errors, "[[access]]", id);
    }
    for id in overlay.naming.overrides.keys() {
        check(errors, "[naming.overrides]", id);
    }
    for id in &overlay.exclude.keep_deprecated {
        check(errors, "exclude.keep_deprecated", id);
    }
    for id in &overlay.exclude.keep_relationship_reads {
        check(errors, "exclude.keep_relationship_reads", id);
    }
    for duplicate in &overlay.exclude.duplicates {
        check(errors, "[[exclude.duplicates]]", &duplicate.operation);
        check(errors, "[[exclude.duplicates]]", &duplicate.replacement);
        if let Some(op) = by_id.get(duplicate.operation.as_str())
            && op.exclusion != Some(Exclusion::DuplicateList)
        {
            errors.push(format!(
                "[[exclude.duplicates]]: {} уже исключена по другой причине",
                duplicate.operation
            ));
        }
        if let Some(replacement) = by_id.get(duplicate.replacement.as_str()) {
            if replacement.exclusion.is_some() {
                errors.push(format!(
                    "[[exclude.duplicates]]: замена {} сама исключена",
                    duplicate.replacement
                ));
            }
            if !replacement
                .params
                .iter()
                .any(|p| p.api_name == duplicate.filter)
            {
                errors.push(format!(
                    "[[exclude.duplicates]]: у {} нет параметра {}",
                    duplicate.replacement, duplicate.filter
                ));
            }
        }
    }
}

/// Имена инструментов зарегистрированных операций.
///
/// 1. Явное имя из `[naming.overrides]` или базовое имя из `operationId`.
/// 2. Если базовое имя совпало у ресурсов разных версий API (`inAppPurchaseImages` и
///    `inAppPurchaseImagesV2`), версия без суффикса оставляет имя, остальные получают `_v2`.
/// 3. Имя длиннее 40 символов сокращается правилами `[naming] abbreviations`.
fn assign_names(overlay: &Overlay, operations: &mut [Operation], errors: &mut Vec<String>) {
    let overrides = &overlay.naming.overrides;
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, op) in operations.iter().enumerate() {
        if op.exclusion.is_some() || overrides.contains_key(&op.operation_id) {
            continue;
        }
        if let Some(name) = &op.name {
            groups.entry(name.base_name()).or_default().push(index);
        }
    }
    for (base, members) in groups {
        // `operationId` без версий у операций группы, у которых версии нет.
        let unversioned: BTreeSet<String> = members
            .iter()
            .filter(|&&i| {
                operations[i]
                    .name
                    .as_ref()
                    .and_then(|n| n.version)
                    .is_none()
            })
            .map(|&i| strip_versions(&operations[i].operation_id))
            .collect();
        for &index in &members {
            let op = &operations[index];
            let version = op.name.as_ref().and_then(|n| n.version);
            // Суффикс — только если в группе есть та же операция без версии;
            // остальные совпадения имён разрешаются явными именами.
            let name = match version {
                Some(version) if unversioned.contains(&strip_versions(&op.operation_id)) => {
                    format!("{base}_v{version}")
                }
                _ => base.clone(),
            };
            operations[index].tool_name = abbreviate(&name, &overlay.naming.abbreviations);
        }
    }
    for op in operations.iter_mut() {
        if let Some(name) = overrides.get(&op.operation_id) {
            op.tool_name = name.clone();
            if op.summary.is_empty() {
                op.summary = format!("{} {}.", op.method.to_uppercase(), op.path);
            }
        }
    }
    for op in operations.iter().filter(|op| op.exclusion.is_none()) {
        if !is_valid_tool_name(&op.tool_name) {
            errors.push(format!(
                "{}: имя инструмента {} ({} символов) недопустимо или длиннее {MAX_TOOL_NAME}: \
                 добавь сокращение в [naming] abbreviations или имя в [naming.overrides]",
                op.operation_id,
                op.tool_name,
                op.tool_name.len()
            ));
        }
    }
}

/// `operationId` без суффиксов версии частей: `inAppPurchaseImagesV2_getInstance` →
/// `inAppPurchaseImages_getInstance`.
fn strip_versions(operation_id: &str) -> String {
    operation_id
        .split('_')
        .map(|part| {
            let digits = part.len() - part.trim_end_matches(|c: char| c.is_ascii_digit()).len();
            match part[..part.len() - digits].strip_suffix('V') {
                Some(base) if digits > 0 && !base.is_empty() => base,
                _ => part,
            }
        })
        .collect::<Vec<_>>()
        .join("_")
}

/// Что вызвать вместо исключённой операции (текст для агента в `api_search`).
fn resolve_alternatives(overlay: &Overlay, operations: &mut [Operation], errors: &mut Vec<String>) {
    let kept_by_id: BTreeMap<String, String> = operations
        .iter()
        .filter(|op| op.exclusion.is_none())
        .map(|op| (op.operation_id.clone(), op.tool_name.clone()))
        .collect();
    let kept_by_route: BTreeMap<(String, &'static str), String> = operations
        .iter()
        .filter(|op| op.exclusion.is_none())
        .map(|op| ((op.path.clone(), op.method), op.tool_name.clone()))
        .collect();
    let duplicates: BTreeMap<&str, &crate::overlay::Duplicate> = overlay
        .exclude
        .duplicates
        .iter()
        .map(|d| (d.operation.as_str(), d))
        .collect();
    let mut duplicate_texts = BTreeMap::new();
    for op in operations.iter() {
        if op.exclusion != Some(Exclusion::DuplicateList) {
            continue;
        }
        let Some(duplicate) = duplicates.get(op.operation_id.as_str()) else {
            continue;
        };
        let Some(replacement) = kept_by_id.get(&duplicate.replacement) else {
            continue;
        };
        let text = format!(
            "Call `{replacement}` with `{}` set to the parent ID.",
            argument_name(&duplicate.filter)
        );
        duplicate_texts.insert(op.path.clone(), text);
    }
    for op in operations.iter_mut() {
        op.alternative = match op.exclusion {
            None => continue,
            Some(Exclusion::DuplicateList) => match duplicate_texts.get(&op.path) {
                Some(text) => text.clone(),
                None => {
                    errors.push(format!("{}: не найдена замена дубликата", op.operation_id));
                    continue;
                }
            },
            Some(Exclusion::RelationshipLinkage) => {
                // `…/{id}/relationships/<связь>` → `…/{id}/<связь>`: тот же список с данными.
                let related = op.path.replacen("/relationships/", "/", 1);
                if let Some(tool) = kept_by_route.get(&(related.clone(), "get")) {
                    format!("Call `{tool}` to get the related resources with their attributes.")
                } else if let Some(text) = duplicate_texts.get(&related) {
                    text.clone()
                } else {
                    String::new()
                }
            }
            Some(Exclusion::Deprecated) => {
                // Замена — та же операция новой версии ресурса (`gameCenterLeaderboardsV2_…`).
                let (resource, rest) = op
                    .operation_id
                    .split_once('_')
                    .unwrap_or((op.operation_id.as_str(), ""));
                (2..=4)
                    .find_map(|version| kept_by_id.get(&format!("{resource}V{version}_{rest}")))
                    .map(|tool| format!("Use `{tool}` (newer version of this resource)."))
                    .unwrap_or_default()
            }
        };
    }
}

/// Описание операции для агента: всё, чего нет в `summary` и схеме аргументов.
fn describe(op: &Operation) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(reason) = &op.access_reason {
        let level = match op.access {
            Level::Read => "Read-only",
            Level::Write => "Write",
            Level::Destructive => "Destructive",
        };
        parts.push(format!("{level}: {reason}"));
    }
    let lists = op
        .name
        .as_ref()
        .is_some_and(|n| matches!(n.action, Action::List | Action::ListRelated));
    if lists && let Some(limit) = op.params.iter().find(|p| p.api_name == "limit") {
        let max = limit
            .schema
            .get("maximum")
            .and_then(Value::as_u64)
            .map(|max| format!(" (max {max})"))
            .unwrap_or_default();
        parts.push(format!(
            "Paginated: `limit` sets the page size{max}; for the next page call `api_request` \
             with the `links.next` URL from the response."
        ));
    }
    if op.params.iter().any(|p| p.api_name == "include") {
        let fields = op.params.iter().any(|p| p.api_name.starts_with("fields["));
        let mut text = "`include` adds related resources to `included`".to_owned();
        if fields {
            text.push_str("; `fields.<type>` limits the attributes returned per resource type");
        }
        text.push('.');
        parts.push(text);
    } else if op.params.iter().any(|p| p.api_name.starts_with("fields[")) {
        parts.push("`fields.<type>` limits the attributes returned.".to_owned());
    }
    if let Some(name) = &op.name
        && matches!(
            name.action,
            Action::Add | Action::Remove | Action::Replace | Action::Set
        )
    {
        parts.push(
            "`body.data` holds resource identifiers (`type` and `id`), not attributes.".to_owned(),
        );
    }
    parts.join("\n\n")
}

/// JSON Schema аргументов инструмента.
fn input_schema(operation_id: &str, params: &[Param]) -> String {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for param in params {
        let mut schema = param.schema.clone();
        let object = schema
            .as_object_mut()
            .expect("схема параметра не является объектом");
        let description = if param.description.is_empty() {
            param.api_name.clone()
        } else {
            param.description.clone()
        };
        object.insert("description".into(), Value::String(description));
        assert!(
            properties.insert(param.name.clone(), schema).is_none(),
            "{operation_id}: аргумент {} повторяется",
            param.name
        );
        if param.required {
            required.push(Value::String(param.name.clone()));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    serde_json::to_string(&sorted(Value::Object(schema))).expect("сериализация JSON Schema")
}

/// Ключи объектов по алфавиту на всех уровнях: результат не зависит от фичи
/// `preserve_order` у `serde_json`, которую может включить любая зависимость.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let entries: BTreeMap<String, Value> = object
                .into_iter()
                .map(|(key, value)| (key, sorted(value)))
                .collect();
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// Имя свойства схемы, которое принимают клиенты: `^[a-zA-Z0-9_.-]{1,64}$` (ограничение API Claude).
fn is_valid_property_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn check_invariants(overlay: &Overlay, operations: &[Operation], errors: &mut Vec<String>) {
    let mut names = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for op in operations {
        assert!(
            ids.insert(op.operation_id.as_str()),
            "operationId {} повторяется",
            op.operation_id
        );
        if op.exclusion.is_some() {
            continue;
        }
        if let Some(other) = names.insert(op.tool_name.as_str(), op.operation_id.as_str()) {
            errors.push(format!(
                "имя инструмента {} у {other} и {}: задай имя в [naming.overrides]",
                op.tool_name, op.operation_id
            ));
        }
        for param in &op.params {
            if !is_valid_property_name(&param.name) {
                errors.push(format!(
                    "{}: недопустимое имя аргумента {}",
                    op.operation_id, param.name
                ));
            }
        }
        if op.summary.is_empty() {
            errors.push(format!("{}: нет summary", op.operation_id));
        }
    }
    for toolset in &overlay.toolsets {
        if !operations
            .iter()
            .any(|op| op.exclusion.is_none() && op.toolset == toolset.slug)
        {
            errors.push(format!("в наборе {} нет операций", toolset.slug));
        }
    }
}

fn rust_method(method: &str) -> &'static str {
    match method {
        "get" => "HttpMethod::Get",
        "post" => "HttpMethod::Post",
        "put" => "HttpMethod::Put",
        "patch" => "HttpMethod::Patch",
        "delete" => "HttpMethod::Delete",
        other => panic!("неподдерживаемый метод {other}"),
    }
}

/// Строковые литералы выводятся через `{:?}`: это корректное экранирование для Rust.
fn emit(overlay: &Overlay, operations: &[Operation], excluded: &[Operation]) -> String {
    let mut out = String::new();
    out.push_str(
        "// Сгенерировано build.rs из spec/openapi.oas.json и spec/overlay.toml. Не редактировать.\n",
    );
    out.push_str("static TOOLSETS: &[ToolsetDef] = &[\n");
    for toolset in &overlay.toolsets {
        let mut tags = toolset.tags.clone();
        tags.sort();
        let _ = writeln!(
            out,
            "    ToolsetDef {{ slug: {:?}, description: {:?}, tags: &{:?} }},",
            toolset.slug,
            toolset.description.trim(),
            tags
        );
    }
    out.push_str("];\n\n");
    out.push_str("static OPERATIONS: &[OperationDef] = &[\n");
    for op in operations {
        let _ = writeln!(out, "    OperationDef {{");
        let _ = writeln!(out, "        tool_name: {:?},", op.tool_name);
        let _ = writeln!(out, "        operation_id: {:?},", op.operation_id);
        let _ = writeln!(out, "        toolset: {:?},", op.toolset);
        let _ = writeln!(out, "        summary: {:?},", op.summary);
        let _ = writeln!(out, "        description: {:?},", op.description);
        let _ = writeln!(out, "        method: {},", rust_method(op.method));
        let _ = writeln!(out, "        path: {:?},", op.path);
        let _ = writeln!(out, "        access: {},", op.access.rust_name());
        let _ = writeln!(out, "        params: &[");
        for param in &op.params {
            let description = if param.description.is_empty() {
                param.api_name.as_str()
            } else {
                param.description.as_str()
            };
            let _ = writeln!(
                out,
                "            ParamDef {{ name: {:?}, api_name: {:?}, location: {}, required: {}, description: {:?}, value_type: ParamType::{} }},",
                param.name,
                param.api_name,
                param.location.rust_name(),
                param.required,
                description,
                param.value_type,
            );
        }
        let _ = writeln!(out, "        ],");
        let body = if op.has_body {
            "Some(BodyKind::Json)"
        } else {
            "None"
        };
        let _ = writeln!(out, "        body: {body},");
        let _ = writeln!(out, "        response: {},", op.response.rust_name());
        let _ = writeln!(out, "        accept: {:?},", op.accept);
        let _ = writeln!(out, "        input_schema: {:?},", op.input_schema);
        let _ = writeln!(out, "    }},");
    }
    out.push_str("];\n\n");
    out.push_str("static EXCLUDED: &[ExcludedOperationDef] = &[\n");
    for op in excluded {
        let reason = op.exclusion.map(Exclusion::rust_name).unwrap_or_default();
        let _ = writeln!(
            out,
            "    ExcludedOperationDef {{ operation_id: {:?}, toolset: {:?}, summary: {:?}, method: {}, path: {:?}, access: {}, reason: {reason}, alternative: {:?} }},",
            op.operation_id,
            op.toolset,
            op.summary,
            rust_method(op.method),
            op.path,
            op.access.rust_name(),
            op.alternative,
        );
    }
    out.push_str("];\n");
    out
}

/// Отчёт для `spec/update_spec.py` и ревью: счётчики, исключения, размеры схем.
fn report(
    overlay: &Overlay,
    operations: &[Operation],
    excluded: &[Operation],
    errors: &[String],
) -> String {
    let mut by_access: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_toolset: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for op in operations {
        *by_access.entry(op.access.as_str()).or_default() += 1;
        by_toolset.entry(&op.toolset).or_default().0 += 1;
    }
    let mut by_reason: BTreeMap<&str, usize> = BTreeMap::new();
    for op in excluded {
        *by_reason
            .entry(op.exclusion.map(Exclusion::as_str).unwrap_or_default())
            .or_default() += 1;
        by_toolset.entry(&op.toolset).or_default().1 += 1;
    }
    let toolsets: Vec<Value> = overlay
        .toolsets
        .iter()
        .map(|t| {
            let (kept, excluded) = by_toolset.get(t.slug.as_str()).copied().unwrap_or_default();
            json!({ "slug": t.slug, "tools": kept, "excluded": excluded })
        })
        .collect();
    let mut sizes: Vec<(usize, &str)> = operations
        .iter()
        .map(|op| (op.input_schema.len(), op.tool_name.as_str()))
        .collect();
    sizes.sort_by(|a, b| b.cmp(a));
    let total_schema: usize = sizes.iter().map(|(size, _)| size).sum();
    let total_description: usize = operations
        .iter()
        .map(|op| op.summary.len() + op.description.len())
        .sum();
    let exceptions: Vec<Value> = operations
        .iter()
        .filter(|op| op.access_reason.is_some())
        .map(|op| json!({ "tool": op.tool_name, "access": op.access.as_str(), "operation_id": op.operation_id }))
        .collect();
    let tools: Vec<Value> = operations
        .iter()
        .map(|op| {
            json!({
                "tool": op.tool_name,
                "operation_id": op.operation_id,
                "method": op.method.to_uppercase(),
                "path": op.path,
                "access": op.access.as_str(),
                "toolset": op.toolset,
                "schema_bytes": op.input_schema.len(),
            })
        })
        .collect();
    let excluded_list: Vec<Value> = excluded
        .iter()
        .map(|op| {
            json!({
                "operation_id": op.operation_id,
                "method": op.method.to_uppercase(),
                "path": op.path,
                "reason": op.exclusion.map(Exclusion::as_str),
                "alternative": op.alternative,
            })
        })
        .collect();
    let report = json!({
        "errors": errors,
        "tools_total": operations.len(),
        "excluded_total": excluded.len(),
        "by_access": by_access,
        "excluded_by_reason": by_reason,
        "toolsets": toolsets,
        "schema_bytes_total": total_schema,
        "description_bytes_total": total_description,
        "largest_schemas": sizes.iter().take(20).map(|(size, name)| json!([name, size])).collect::<Vec<_>>(),
        "access_exceptions": exceptions,
        "tools": tools,
        "excluded": excluded_list,
    });
    serde_json::to_string_pretty(&report).expect("сериализация отчёта")
}
