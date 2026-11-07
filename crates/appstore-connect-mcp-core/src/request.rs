//! Сборка запроса к API из аргументов инструмента операции.
//!
//! Порядок описан в документации `appstore_connect_api::spec`:
//!
//! - аргументы называются [`ParamDef::name`] (`filter.app`, `fields.apps`), в строку запроса
//!   уходят под [`ParamDef::api_name`] (`filter[app]`, `fields[apps]`);
//! - неизвестные аргументы отклоняются, обязательные проверяются, значения приводятся по
//!   [`ParamType`] и проверяются по `enum` схемы операции — всё до отправки запроса;
//! - массивы JSON:API (`include`, `fields[…]`, `filter[…]`) передаются одним значением через
//!   запятую ([`query_value`]); одиночное значение вместо массива допускается;
//! - тело — аргумент `body` (JSON-объект или строка с ним);
//! - значение `null` считается непереданным аргументом: некоторые клиенты так передают
//!   необязательные параметры.
//!
//! Добавления ядра к аргументам операции: [`SAVE_TO_ARG`] (обязателен у операций с двоичным
//! ответом: отчёты продаж и финансов) и [`FLATTEN_ARG`] (у операций с `include`). Файл
//! `save_to` проходит [`FilePolicy`].

use std::collections::HashMap;

use appstore_connect_api::executor::{ApiRequest, RequestBody, query_value, render_path};
use appstore_connect_api::spec::{OperationDef, ParamDef, ParamLocation, ParamType, ResponseKind};
use rmcp::model::JsonObject;
use serde_json::Value;

use crate::error::ToolError;
use crate::files::FilePolicy;

/// Аргумент инструментов с двоичным ответом: куда сохранить файл.
pub const SAVE_TO_ARG: &str = "save_to";

/// Аргумент инструментов с `include`: подставить связанные объекты на место ссылок.
pub const FLATTEN_ARG: &str = "flatten";

/// Принимает ли инструмент операции аргумент [`SAVE_TO_ARG`].
pub fn takes_save_to(op: &OperationDef) -> bool {
    op.response == ResponseKind::Binary
}

/// Принимает ли инструмент операции аргумент [`FLATTEN_ARG`]: JSON-ответ и параметр
/// `include`.
pub fn takes_flatten(op: &OperationDef) -> bool {
    op.response == ResponseKind::Json
        && op
            .param("include")
            .is_some_and(|param| param.location == ParamLocation::Query)
}

/// Собранный вызов операции.
#[derive(Debug, Clone)]
pub struct OperationCall {
    pub request: ApiRequest,
    /// Агент попросил `flatten: true`.
    pub flatten: bool,
}

/// Собирает запрос для операции `op` из аргументов инструмента.
///
/// `schema` — схема аргументов инструмента (`input_schema` операции): из неё берутся
/// допустимые значения (`enum`).
pub fn build_request(
    op: &OperationDef,
    schema: &JsonObject,
    args: &JsonObject,
    files: &FilePolicy,
) -> Result<OperationCall, ToolError> {
    let present = |name: &str| args.get(name).is_some_and(|value| !value.is_null());

    reject_unknown(op, args)?;
    let mut missing: Vec<String> = op
        .params
        .iter()
        .filter(|param| param.required && !present(param.name))
        .map(|param| format!("`{}`", param.name))
        .collect();
    if takes_save_to(op) && !present(SAVE_TO_ARG) {
        missing.push(format!("`{SAVE_TO_ARG}`"));
    }
    if !missing.is_empty() {
        return Err(ToolError::new(format!(
            "missing required argument(s): {}",
            missing.join(", ")
        )));
    }

    let mut path_values: HashMap<&str, String> = HashMap::new();
    let mut query = Vec::new();
    let mut body = RequestBody::Empty;
    for param in op.params {
        let Some(value) = args.get(param.name).filter(|value| !value.is_null()) else {
            continue;
        };
        check_enum(schema, param, value)?;
        match param.location {
            ParamLocation::Path => {
                path_values.insert(param.api_name, scalar(param, value)?);
            }
            ParamLocation::Query => {
                if let Some(item) = query_param(param, value)? {
                    query.push((param.api_name.to_owned(), item));
                }
            }
            ParamLocation::Body => body = RequestBody::Json(json_body(param, value)?),
        }
    }

    let path = render_path(op.path, |name| path_values.get(name).cloned())?;
    let save_to = match args.get(SAVE_TO_ARG).filter(|value| !value.is_null()) {
        Some(value) if takes_save_to(op) => Some(save_to_path(value, files)?),
        _ => None,
    };
    let flatten = match args.get(FLATTEN_ARG).filter(|value| !value.is_null()) {
        Some(value) if takes_flatten(op) => boolean(FLATTEN_ARG, value)?,
        _ => false,
    };

    let mut request = ApiRequest::new(op.method, path);
    request.query = query;
    request.body = body;
    request.response = op.response;
    request.save_to = save_to;
    Ok(OperationCall { request, flatten })
}

/// Путь `save_to`: новый файл внутри разрешённых каталогов.
pub fn save_to_path(value: &Value, files: &FilePolicy) -> Result<std::path::PathBuf, ToolError> {
    let Value::String(target) = value else {
        return Err(ToolError::new(format!(
            "argument `{SAVE_TO_ARG}` must be a path to a new local file, got {}",
            kind(value)
        )));
    };
    files
        .check_write(target)
        .map_err(|err| ToolError::new(format!("argument `{SAVE_TO_ARG}`: {err}")))
}

/// Логическое значение аргумента ядра (`flatten`).
pub fn boolean(name: &str, value: &Value) -> Result<bool, ToolError> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s.trim().eq_ignore_ascii_case("true") => Ok(true),
        Value::String(s) if s.trim().eq_ignore_ascii_case("false") => Ok(false),
        other => Err(ToolError::new(format!(
            "argument `{name}` must be a boolean, got {}",
            kind(other)
        ))),
    }
}

/// Проверяет значение аргумента операции заранее, без сборки запроса: так пользовательский
/// инструмент с фиксированными аргументами (`tools.toml`) отклоняется при загрузке, а не при
/// первом вызове. `save_to` фиксировать нельзя: путь проверяется при каждом вызове.
pub fn check_fixed_argument(
    op: &OperationDef,
    schema: &JsonObject,
    name: &str,
    value: &Value,
) -> Result<(), ToolError> {
    if name == SAVE_TO_ARG && takes_save_to(op) {
        return Err(ToolError::new(format!(
            "argument `{SAVE_TO_ARG}` cannot be fixed: the agent passes a new file path on every \
             call"
        )));
    }
    if name == FLATTEN_ARG && takes_flatten(op) {
        return boolean(name, value).map(|_| ());
    }
    let Some(param) = op.param(name) else {
        let names: Vec<String> = op
            .params
            .iter()
            .map(|param| format!("`{}`", param.name))
            .collect();
        return Err(ToolError::new(format!(
            "operation `{}` has no argument `{name}`; its arguments: {}",
            op.tool_name,
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            }
        )));
    };
    if value.is_null() {
        return Err(ToolError::new(format!(
            "argument `{name}` cannot be fixed to null; remove it instead"
        )));
    }
    check_enum(schema, param, value)?;
    match param.location {
        ParamLocation::Path => scalar(param, value).map(|_| ()),
        ParamLocation::Query => query_param(param, value).map(|_| ()),
        ParamLocation::Body => json_body(param, value).map(|_| ()),
    }
}

/// Имена аргументов инструмента операции: параметры и добавления ядра.
pub fn argument_names(op: &OperationDef) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = op.params.iter().map(|param| param.name).collect();
    if takes_save_to(op) {
        names.push(SAVE_TO_ARG);
    }
    if takes_flatten(op) {
        names.push(FLATTEN_ARG);
    }
    names
}

fn reject_unknown(op: &OperationDef, args: &JsonObject) -> Result<(), ToolError> {
    let names = argument_names(op);
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !names.contains(&key.as_str()))
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let allowed = if names.is_empty() {
        "none".to_owned()
    } else {
        names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    // Частая ошибка — запись JSON:API вместо имени аргумента: `filter[app]` вместо
    // `filter.app`.
    let hint = if unknown.iter().any(|key| key.contains('[')) {
        " (use dotted names: `filter.app`, not `filter[app]`)"
    } else {
        ""
    };
    Err(ToolError::new(format!(
        "unknown argument(s) {}{hint}; allowed arguments: {allowed}",
        unknown.join(", ")
    )))
}

/// Допустимые значения параметра из схемы инструмента: `enum` свойства или его `items`.
fn allowed_values<'a>(schema: &'a JsonObject, name: &str) -> Option<&'a Vec<Value>> {
    let property = schema.get("properties")?.get(name)?;
    property
        .get("enum")
        .or_else(|| property.get("items").and_then(|items| items.get("enum")))
        .and_then(Value::as_array)
}

fn check_enum(schema: &JsonObject, param: &ParamDef, value: &Value) -> Result<(), ToolError> {
    let Some(allowed) = allowed_values(schema, param.name) else {
        return Ok(());
    };
    let items: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for item in items {
        // Числа и логические значения сравниваются текстом: `enum` бывает и у них.
        let matches = allowed
            .iter()
            .any(|candidate| candidate == item || text(candidate) == text(item));
        if !matches {
            let list: Vec<String> = allowed.iter().map(Value::to_string).collect();
            return Err(ToolError::new(format!(
                "argument `{}`: {} is not an allowed value; allowed values: {}",
                param.name,
                item,
                list.join(", ")
            )));
        }
    }
    Ok(())
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Значение path- или query-параметра строкой.
fn scalar(param: &ParamDef, value: &Value) -> Result<String, ToolError> {
    let invalid = |expected: &str| {
        ToolError::new(format!(
            "argument `{}` must be {expected}, got {}",
            param.name,
            kind(value)
        ))
    };
    match param.value_type {
        ParamType::Integer => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => Ok(n.to_string()),
            Value::String(s) if is_integer(s.trim()) => Ok(s.trim().to_owned()),
            _ => Err(invalid("an integer (a number or a string of digits)")),
        },
        ParamType::Number => match value {
            Value::Number(n) => Ok(n.to_string()),
            Value::String(s) if s.trim().parse::<f64>().is_ok_and(f64::is_finite) => {
                Ok(s.trim().to_owned())
            }
            _ => Err(invalid("a number")),
        },
        ParamType::Boolean => match value {
            Value::Bool(b) => Ok(b.to_string()),
            Value::String(s) if s.trim().eq_ignore_ascii_case("true") => Ok("true".to_owned()),
            Value::String(s) if s.trim().eq_ignore_ascii_case("false") => Ok("false".to_owned()),
            _ => Err(invalid("a boolean")),
        },
        // Строки и элементы массивов: числа и логические значения принимаем как их
        // текстовую запись — ID в App Store Connect бывают числовыми.
        ParamType::String | ParamType::Array => match value {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(n.to_string()),
            Value::Bool(b) => Ok(b.to_string()),
            _ => Err(invalid("a string")),
        },
        ParamType::Object => Err(invalid("a string")),
    }
}

/// Значение query-параметра: массив — через запятую, одиночное значение массива допустимо.
fn query_param(param: &ParamDef, value: &Value) -> Result<Option<String>, ToolError> {
    match (param.value_type, value) {
        (ParamType::Array, Value::Array(items)) => {
            for item in items {
                scalar(param, item)?;
            }
            query_value(param.api_name, value)
                .map_err(|_| {
                    ToolError::new(format!(
                        "argument `{}`: array items must be strings, numbers or booleans",
                        param.name
                    ))
                })
                // Пустой массив — параметра нет: `include=` API отклоняет.
                .map(|joined| joined.filter(|joined| !joined.is_empty()))
        }
        _ => scalar(param, value).map(Some),
    }
}

/// JSON-тело. Строка с JSON допускается: исполнитель разберёт её и отправит объектом
/// (некоторые клиенты передают тело строкой).
fn json_body(param: &ParamDef, value: &Value) -> Result<Value, ToolError> {
    let parsed = match value {
        Value::String(text) => serde_json::from_str::<Value>(text).ok(),
        other => Some(other.clone()),
    };
    let ok = match (param.value_type, &parsed) {
        (ParamType::Array, Some(Value::Array(_))) => true,
        (ParamType::Array, _) => false,
        (_, Some(Value::Object(_))) => true,
        _ => false,
    };
    if !ok {
        let expected = match param.value_type {
            ParamType::Array => "a JSON array",
            _ => "a JSON object",
        };
        return Err(ToolError::new(format!(
            "argument `{}` must be {expected} (the request body, for example \
             `{{\"data\": {{\"type\": ..., \"attributes\": {{...}}}}}}`), got {}",
            param.name,
            kind(value)
        )));
    }
    Ok(value.clone())
}

fn is_integer(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Вид JSON-значения для сообщений.
pub fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use appstore_connect_api::HttpMethod;
    use appstore_connect_api::spec::{find, operations};
    use serde_json::json;

    use super::*;

    fn op(name: &str) -> &'static OperationDef {
        find(name).unwrap_or_else(|| panic!("нет операции {name}"))
    }

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            other => panic!("не объект: {other}"),
        }
    }

    fn schema(op: &OperationDef) -> JsonObject {
        serde_json::from_str(op.input_schema).unwrap()
    }

    fn policy() -> FilePolicy {
        FilePolicy::new(&[]).unwrap()
    }

    fn build(name: &str, value: Value) -> Result<OperationCall, ToolError> {
        let op = op(name);
        build_request(op, &schema(op), &args(value), &policy())
    }

    #[test]
    fn path_and_query_use_api_names() {
        let call = build(
            "get_app",
            json!({
                "id": "123",
                "include": ["appInfos", "builds"],
                "fields.apps": ["name", "bundleId"],
                "limit.builds": 5,
                "fields.builds": null
            }),
        )
        .unwrap();
        let request = call.request;
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/v1/apps/123");
        assert_eq!(
            request.query,
            vec![
                ("fields[apps]".to_owned(), "name,bundleId".to_owned()),
                ("include".to_owned(), "appInfos,builds".to_owned()),
                ("limit[builds]".to_owned(), "5".to_owned()),
            ]
        );
        assert!(matches!(request.body, RequestBody::Empty));
        assert!(request.save_to.is_none());
        assert!(!call.flatten);
    }

    #[test]
    fn single_value_for_array_and_empty_array() {
        let request = build("get_app", json!({"id": 1, "include": "builds"}))
            .unwrap()
            .request;
        assert_eq!(request.path, "/v1/apps/1");
        assert_eq!(request.query, vec![("include".into(), "builds".into())]);
        let request = build("get_app", json!({"id": "1", "include": []}))
            .unwrap()
            .request;
        assert!(request.query.is_empty());
    }

    #[test]
    fn path_values_are_single_segments() {
        let request = build("get_app", json!({"id": "a/b c"})).unwrap().request;
        assert_eq!(request.path, "/v1/apps/a%2Fb%20c");
        let err = build("get_app", json!({"id": ".."})).unwrap_err();
        assert!(err.message().contains("valid API path"), "{err}");
        let err = build("get_app", json!({"id": ""})).unwrap_err();
        assert!(err.message().contains("missing path argument"), "{err}");
        let err = build("get_app", json!({"id": {"a": 1}})).unwrap_err();
        assert!(
            err.message().contains("must be a string, got an object"),
            "{err}"
        );
    }

    #[test]
    fn missing_unknown_and_enum() {
        let err = build("get_app", json!({})).unwrap_err();
        assert_eq!(err.message(), "missing required argument(s): `id`");
        let err = build("get_app", json!({"id": null})).unwrap_err();
        assert!(err.message().contains("`id`"), "{err}");

        let err = build("get_app", json!({"id": "1", "fields[apps]": "name"})).unwrap_err();
        assert!(
            err.message()
                .starts_with("unknown argument(s) `fields[apps]`"),
            "{err}"
        );
        assert!(err.message().contains("dotted names"), "{err}");
        assert!(err.message().contains("`include`"), "{err}");
        let err = build("get_app", json!({"id": "1", "save_to": "/tmp/x"})).unwrap_err();
        assert!(
            err.message().contains("unknown argument(s) `save_to`"),
            "{err}"
        );

        let err = build("get_app", json!({"id": "1", "include": ["nope"]})).unwrap_err();
        assert!(err.message().contains("not an allowed value"), "{err}");
        assert!(err.message().contains("\"builds\""), "{err}");
    }

    #[test]
    fn scalar_types() {
        let op = op("list_apps");
        let limit = op.param("limit").unwrap();
        assert_eq!(scalar(limit, &json!(5)).unwrap(), "5");
        assert_eq!(scalar(limit, &json!(" 7 ")).unwrap(), "7");
        for bad in [json!(1.5), json!("ten"), json!(true)] {
            let err = scalar(limit, &bad).unwrap_err();
            assert!(err.message().contains("must be an integer"), "{err}");
        }
        let boolean_param = ParamDef {
            name: "exists.x",
            api_name: "exists[x]",
            location: ParamLocation::Query,
            required: false,
            description: "",
            value_type: ParamType::Boolean,
        };
        assert_eq!(scalar(&boolean_param, &json!("FALSE")).unwrap(), "false");
        assert!(scalar(&boolean_param, &json!(1)).is_err());
        let number = ParamDef {
            value_type: ParamType::Number,
            ..boolean_param
        };
        assert_eq!(scalar(&number, &json!("2.5")).unwrap(), "2.5");
        assert!(scalar(&number, &json!("NaN")).is_err());
    }

    #[test]
    fn json_body() {
        let op = operations()
            .iter()
            .find(|op| op.tool_name.starts_with("create_") && op.param("body").is_some())
            .unwrap();
        let mut value = JsonObject::new();
        for param in op.params.iter().filter(|p| p.required && p.name != "body") {
            value.insert(param.name.to_owned(), json!("x"));
        }
        value.insert("body".into(), json!({"data": {"type": "x"}}));
        let request = build_request(op, &schema(op), &value, &policy())
            .unwrap()
            .request;
        assert!(matches!(request.body, RequestBody::Json(ref v) if v["data"]["type"] == "x"));
        value.insert("body".into(), json!("{\"data\": {}}"));
        let request = build_request(op, &schema(op), &value, &policy())
            .unwrap()
            .request;
        assert!(matches!(request.body, RequestBody::Json(Value::String(_))));
        for bad in [json!("not json"), json!([1]), json!(5)] {
            value.insert("body".into(), bad);
            let err = build_request(op, &schema(op), &value, &policy()).unwrap_err();
            assert!(err.message().contains("must be a JSON object"), "{err}");
        }
    }

    #[test]
    fn flatten_argument() {
        assert!(takes_flatten(op("get_app")));
        assert!(!takes_flatten(op("list_sandbox_testers")));
        let call = build("get_app", json!({"id": "1", "flatten": true})).unwrap();
        assert!(call.flatten);
        let err = build("get_app", json!({"id": "1", "flatten": 3})).unwrap_err();
        assert!(err.message().contains("must be a boolean"), "{err}");
        let err = build("list_sandbox_testers", json!({"flatten": true})).unwrap_err();
        assert!(
            err.message().contains("unknown argument(s) `flatten`"),
            "{err}"
        );
    }

    #[test]
    fn binary_response_needs_save_to_inside_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let policy = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
        let download = operations()
            .iter()
            .find(|op| op.path == "/v1/salesReports")
            .unwrap();
        assert!(takes_save_to(download));
        let base = json!({
            "filter.vendorNumber": "8000",
            "filter.reportType": "SALES",
            "filter.reportSubType": ["SUMMARY"],
            "filter.frequency": "DAILY"
        });
        let schema = schema(download);

        let err = build_request(download, &schema, &args(base.clone()), &policy).unwrap_err();
        assert_eq!(err.message(), "missing required argument(s): `save_to`");

        let mut with_target = args(base.clone());
        let target = root.join("sales.tsv.gz");
        with_target.insert(SAVE_TO_ARG.into(), json!(target.display().to_string()));
        let request = build_request(download, &schema, &with_target, &policy)
            .unwrap()
            .request;
        assert_eq!(request.path, "/v1/salesReports");
        assert_eq!(request.response, ResponseKind::Binary);
        assert_eq!(request.save_to, Some(target.clone()));
        assert!(
            request
                .query
                .contains(&("filter[reportType]".into(), "SALES".into()))
        );

        std::fs::write(&target, b"old").unwrap();
        let err = build_request(download, &schema, &with_target, &policy).unwrap_err();
        assert!(err.message().contains("already exists"), "{err}");
        let other = tempfile::tempdir().unwrap();
        let mut outside = args(base);
        outside.insert(
            SAVE_TO_ARG.into(),
            json!(other.path().join("x.gz").display().to_string()),
        );
        let err = build_request(download, &schema, &outside, &policy).unwrap_err();
        assert!(err.message().contains("argument `save_to`"), "{err}");
        assert!(
            err.message().contains("outside the allowed directories"),
            "{err}"
        );
    }

    #[test]
    fn fixed_arguments() {
        let op = op("get_app");
        let schema = schema(op);
        assert!(check_fixed_argument(op, &schema, "include", &json!(["builds"])).is_ok());
        assert!(check_fixed_argument(op, &schema, "flatten", &json!(true)).is_ok());
        assert!(check_fixed_argument(op, &schema, "include", &json!(["nope"])).is_err());
        assert!(check_fixed_argument(op, &schema, "id", &Value::Null).is_err());
        let err = check_fixed_argument(op, &schema, "nope", &json!(1)).unwrap_err();
        assert!(err.message().contains("has no argument `nope`"), "{err}");
    }

    #[test]
    fn no_operation_parameter_clashes_with_core_arguments() {
        for op in operations() {
            for name in [SAVE_TO_ARG, FLATTEN_ARG, "jq"] {
                assert!(op.param(name).is_none(), "{} {name}", op.tool_name);
            }
            // Схема операции описывает ровно её параметры.
            let schema = schema(op);
            let properties = schema["properties"].as_object().unwrap();
            assert_eq!(properties.len(), op.params.len(), "{}", op.tool_name);
            for param in op.params {
                assert!(properties.contains_key(param.name), "{}", op.tool_name);
            }
        }
    }
}
