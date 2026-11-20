//! Инструмент `api_request`: запрос к любому пути App Store Connect API.
//!
//! Нужен для операций без своего инструмента (исключённые операции, наборы, которые не
//! включены) и для постраничной выдачи: следующая страница — это адрес `links.next`.
//!
//! Правила:
//!
//! - `path` — относительный путь `/v1/…` (`/v2/…`, `/v3/…`), query можно дописать после `?`
//!   или передать объектом `query`; либо полный адрес `links.next` из ответа API — тогда
//!   только `GET` и без `query`. Другие полные адреса, `..`, `//` и прочее, что не проходит
//!   проверку пути исполнителя, отклоняются: запрос не уходит на другой хост.
//! - Заголовков аргументы не принимают: `Authorization` (JWT профиля) добавляет исполнитель,
//!   и переопределить его нельзя.
//! - Уровень доступа — по операции, которой соответствует запрос (включая исключённые
//!   операции), иначе по методу ([`crate::routes`]). Запрос выше уровня сервера отклоняется
//!   с объяснением, какой флаг нужен. Наборы инструментов `api_request` не ограничивают.
//! - Тело — `body` (JSON-объект) или `body_file` (локальный JSON-файл по [`FilePolicy`]).
//! - `save_to` — сохранить ответ в новый локальный файл: двоичный ответ (отчёты) пишет
//!   исполнитель, JSON — сервер после постобработки.
//! - `jq` — фильтр JSON-ответа ([`crate::jq`]); сервер извлекает его до [`prepare`], с
//!   `save_to` не сочетается.

use std::path::PathBuf;
use std::sync::Arc;

use appstore_connect_api::executor::{ApiClient, ApiRequest, RequestBody, query_value};
use appstore_connect_api::path::validate_relative_path;
use appstore_connect_api::spec::{AccessLevel, HttpMethod, ResponseKind};
use percent_encoding::percent_decode_str;
use rmcp::model::{JsonObject, Tool};
use serde_json::{Value, json};

use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::jq::add_jq_argument;
use crate::registry::{access_hint, access_name, access_verb, annotations};
use crate::request::{SAVE_TO_ARG, kind, save_to_path};
use crate::routes::{RouteMatch, route};

/// Имя инструмента.
pub const API_REQUEST_TOOL: &str = "api_request";

/// Наибольший размер `body_file`.
pub const MAX_BODY_FILE_BYTES: u64 = 4 * 1024 * 1024;

const ARGS: &[&str] = &["method", "path", "query", "body", "body_file", SAVE_TO_ARG];

/// Описание инструмента; аннотации — по максимальному уровню сервера.
pub fn api_request_tool(max_access: AccessLevel, local_files: bool) -> Tool {
    let methods: Vec<&str> = match max_access {
        AccessLevel::Read => vec!["GET"],
        AccessLevel::Write => vec!["GET", "POST", "PATCH", "PUT"],
        AccessLevel::Destructive => vec!["GET", "POST", "PATCH", "PUT", "DELETE"],
    };
    let mut properties = JsonObject::new();
    properties.insert(
        "method".to_owned(),
        json!({
            "type": "string",
            "enum": ["GET", "POST", "PATCH", "PUT", "DELETE"],
            "description": "HTTP method. Default GET."
        }),
    );
    properties.insert(
        "path".to_owned(),
        json!({
            "type": "string",
            "description": "Relative API path such as `/v1/apps/123/appStoreVersions` (query \
                may follow `?`), or the full `links.next` / `pagination.next` URL of a previous \
                response to get the next page."
        }),
    );
    properties.insert(
        "query".to_owned(),
        json!({
            "type": "object",
            "description": "Query parameters: `{\"filter[platform]\": [\"IOS\"], \"limit\": 50}`. \
                Dotted names (`filter.platform`) are accepted too; arrays are sent \
                comma-separated.",
            "additionalProperties": {"type": ["string", "number", "boolean", "array"]}
        }),
    );
    properties.insert(
        "body".to_owned(),
        json!({
            "type": "object",
            "description": "JSON request body (`{\"data\": {...}}`)."
        }),
    );
    if local_files {
        properties.insert(
            "body_file".to_owned(),
            json!({
                "type": "string",
                "description": "Path of a local JSON file with the request body, inside the \
                    directories allowed by the server. Use instead of `body`."
            }),
        );
        properties.insert(
            SAVE_TO_ARG.to_owned(),
            json!({
                "type": "string",
                "description": "Save the response to this new local file instead of returning \
                    it (required for binary responses such as sales and finance reports)."
            }),
        );
    }
    let mut schema = JsonObject::new();
    schema.insert("type".to_owned(), json!("object"));
    schema.insert("additionalProperties".to_owned(), json!(false));
    schema.insert("required".to_owned(), json!(["path"]));
    schema.insert("properties".to_owned(), Value::Object(properties));
    add_jq_argument(&mut schema);
    let description = format!(
        "Call any App Store Connect API path within this server's access level ({}: {}). Use it \
         for operations without a dedicated tool (`api_search` finds them, including excluded \
         ones), for toolsets that are not enabled, and for the next page of a list: pass the \
         `links.next` (or `pagination.next`) URL as `path`. Prefer the dedicated tool when one \
         is registered: it validates arguments and documents the schema. The access level is \
         taken from the operation the request matches; requests above the server's level are \
         rejected. Authorization is added by the server and cannot be overridden. `jq` filters \
         a JSON response (not together with `save_to`).",
        access_name(max_access),
        methods.join(", "),
    );
    let mut tool = Tool::new(API_REQUEST_TOOL, description, Arc::new(schema));
    tool.title = Some("App Store Connect API request".to_owned());
    tool.annotations = Some(annotations(
        "App Store Connect API request",
        max_access,
        None,
    ));
    tool
}

/// Подготовленный запрос `api_request`.
#[derive(Debug)]
pub struct Prepared {
    pub request: ApiRequest,
    pub matched: Option<RouteMatch>,
    pub access: AccessLevel,
    /// JSON-ответ сохранить в этот файл (двоичный ответ сохраняет исполнитель).
    pub json_save_to: Option<PathBuf>,
}

/// Проверяет аргументы и собирает запрос.
pub fn prepare(
    args: &JsonObject,
    client: &ApiClient,
    max_access: AccessLevel,
    files: &FilePolicy,
) -> Result<Prepared, ToolError> {
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !ARGS.contains(&key.as_str()))
        .map(|key| format!("`{key}`"))
        .collect();
    if !unknown.is_empty() {
        let hint = if unknown
            .iter()
            .any(|k| k.to_ascii_lowercase().contains("header"))
        {
            " (headers cannot be set: the server adds Authorization itself)"
        } else {
            ""
        };
        return Err(ToolError::new(format!(
            "unknown argument(s) {}{hint}; allowed arguments: {}",
            unknown.join(", "),
            ARGS.iter()
                .map(|a| format!("`{a}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let get = |name: &str| args.get(name).filter(|v| !v.is_null());
    let method = match get("method") {
        None => HttpMethod::Get,
        Some(Value::String(m)) => parse_method(m)?,
        Some(other) => {
            return Err(ToolError::new(format!(
                "argument `method` must be a string, got {}",
                kind(other)
            )));
        }
    };
    let Some(Value::String(raw_path)) = get("path") else {
        return Err(ToolError::new(
            "argument `path` is required: a relative API path such as `/v1/apps` or a \
             `links.next` URL",
        ));
    };
    let raw_path = raw_path.trim();

    let mut request = if raw_path.contains("://") {
        if method != HttpMethod::Get {
            return Err(ToolError::new(
                "a full URL is accepted only as a `links.next` page link with method GET; use a \
                 relative path such as `/v1/...` for other requests",
            ));
        }
        if get("query").is_some() {
            return Err(ToolError::new(
                "`query` cannot be combined with a `links.next` URL: the link already has the \
                 query of the next page",
            ));
        }
        client.next_page_request(raw_path).map_err(|err| {
            ToolError::new(format!(
                "`path` must be a relative API path (`/v1/...`) or the `links.next` URL from an \
                 App Store Connect API response: {err}"
            ))
        })?
    } else {
        let (path, inline) = match raw_path.split_once('?') {
            Some((path, query)) => (path, parse_query(query)?),
            None => (raw_path, Vec::new()),
        };
        validate_relative_path(path).map_err(ToolError::from)?;
        let mut request = ApiRequest::new(method, path);
        request.query = inline;
        if let Some(query) = get("query") {
            request.query.extend(query_object(query)?);
        }
        request
    };

    // Уровень доступа — по операции, которой соответствует путь.
    let route = route(method, &request.path);
    if !max_access.allows(route.access) {
        return Err(ToolError::new(format!(
            "`{} {}` {} data in App Store Connect and needs {} access, but the server runs with \
             {} access. Ask the user to {} if this change is intended",
            method.as_str(),
            request.path,
            access_verb(route.access),
            access_name(route.access),
            access_name(max_access),
            access_hint(route.access),
        )));
    }

    let body = match (get("body"), get("body_file")) {
        (Some(_), Some(_)) => {
            return Err(ToolError::new(
                "pass either `body` or `body_file`, not both",
            ));
        }
        (Some(body), None) => Some(parse_body(body)?),
        (None, Some(Value::String(file))) => Some(read_body_file(file, files)?),
        (None, Some(other)) => {
            return Err(ToolError::new(format!(
                "argument `body_file` must be a path to a local JSON file, got {}",
                kind(other)
            )));
        }
        (None, None) => None,
    };
    if let Some(body) = body {
        if method == HttpMethod::Get {
            return Err(ToolError::new("a GET request cannot have a body"));
        }
        request.body = RequestBody::Json(body);
    }

    let binary = matches!(
        route.matched,
        Some(RouteMatch::Operation(op)) if op.response == ResponseKind::Binary
    );
    let save_to = match get(SAVE_TO_ARG) {
        Some(value) => Some(save_to_path(value, files)?),
        None => None,
    };
    let mut json_save_to = None;
    if binary {
        let Some(save_to) = save_to else {
            return Err(ToolError::new(format!(
                "the response of `{} {}` is a file (for example a gzip report): pass `{SAVE_TO_ARG}` \
                 with a path of a new local file",
                method.as_str(),
                request.path
            )));
        };
        request.response = ResponseKind::Binary;
        request.save_to = Some(save_to);
    } else {
        json_save_to = save_to;
    }

    Ok(Prepared {
        request,
        matched: route.matched,
        access: route.access,
        json_save_to,
    })
}

fn parse_method(method: &str) -> Result<HttpMethod, ToolError> {
    match method.trim().to_ascii_uppercase().as_str() {
        "GET" => Ok(HttpMethod::Get),
        "POST" => Ok(HttpMethod::Post),
        "PATCH" => Ok(HttpMethod::Patch),
        "PUT" => Ok(HttpMethod::Put),
        "DELETE" => Ok(HttpMethod::Delete),
        other => Err(ToolError::new(format!(
            "unsupported method `{other}`; use GET, POST, PATCH, PUT or DELETE"
        ))),
    }
}

/// Query из строки после `?`: пары `ключ=значение`, percent-кодирование раскрывается.
fn parse_query(query: &str) -> Result<Vec<(String, String)>, ToolError> {
    let decode = |text: &str| {
        percent_decode_str(&text.replace('+', " "))
            .decode_utf8()
            .map(|cow| cow.into_owned())
            .map_err(|_| ToolError::new("the query string of `path` is not valid UTF-8"))
    };
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Ok((decode(key)?, decode(value)?))
        })
        .collect()
}

/// Query из объекта: точечные имена (`filter.app`) переводятся в запись JSON:API.
fn query_object(value: &Value) -> Result<Vec<(String, String)>, ToolError> {
    let Value::Object(map) = value else {
        return Err(ToolError::new(format!(
            "argument `query` must be an object of query parameters, got {}",
            kind(value)
        )));
    };
    let mut pairs = Vec::new();
    for (key, value) in map {
        let key = bracket_name(key);
        if let Some(text) = query_value(&key, value).map_err(ToolError::from)? {
            pairs.push((key, text));
        }
    }
    Ok(pairs)
}

/// `filter.app` → `filter[app]`; имена со скобками и без точки не меняются.
fn bracket_name(key: &str) -> String {
    if key.contains('[') {
        return key.to_owned();
    }
    match key.split_once('.') {
        Some((head, tail)) if !head.is_empty() && !tail.is_empty() => format!("{head}[{tail}]"),
        _ => key.to_owned(),
    }
}

fn parse_body(value: &Value) -> Result<Value, ToolError> {
    let parsed = match value {
        Value::String(text) => serde_json::from_str::<Value>(text).ok(),
        other => Some(other.clone()),
    };
    match parsed {
        Some(Value::Object(_)) => Ok(value.clone()),
        _ => Err(ToolError::new(format!(
            "argument `body` must be a JSON object (the request body), got {}",
            kind(value)
        ))),
    }
}

fn read_body_file(path: &str, files: &FilePolicy) -> Result<Value, ToolError> {
    let checked = files
        .check_read(path)
        .map_err(|err| ToolError::new(format!("argument `body_file`: {err}")))?;
    let size = std::fs::metadata(&checked)
        .map_err(|err| {
            ToolError::new(format!("argument `body_file`: cannot read `{path}`: {err}"))
        })?
        .len();
    if size > MAX_BODY_FILE_BYTES {
        return Err(ToolError::new(format!(
            "argument `body_file`: `{path}` is larger than {MAX_BODY_FILE_BYTES} bytes"
        )));
    }
    let text = std::fs::read_to_string(&checked).map_err(|err| {
        ToolError::new(format!("argument `body_file`: cannot read `{path}`: {err}"))
    })?;
    match serde_json::from_str::<Value>(&text) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err(ToolError::new(format!(
            "argument `body_file`: `{path}` must contain a JSON object"
        ))),
        Err(err) => Err(ToolError::new(format!(
            "argument `body_file`: `{path}` is not valid JSON: {err}"
        ))),
    }
}

/// Сохраняет JSON-ответ в новый файл `target` (проверенный [`FilePolicy::check_write`]).
/// Файл создаётся с `create_new`; после создания проверяется, что каталог не подменили.
pub async fn write_json_file(target: &std::path::Path, text: &str) -> Result<u64, ToolError> {
    use tokio::io::AsyncWriteExt;

    let parent = target
        .parent()
        .ok_or_else(|| ToolError::new("`save_to` has no parent directory"))?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .await
        .map_err(|err| {
            ToolError::new(format!(
                "cannot create `{}`: {err} (existing files are never overwritten)",
                target.display()
            ))
        })?;
    let same_dir = tokio::fs::canonicalize(parent)
        .await
        .is_ok_and(|canonical| canonical == parent);
    if !same_dir {
        let _ = tokio::fs::remove_file(target).await;
        return Err(ToolError::new(
            "the directory of `save_to` changed while the file was being created; nothing was \
             saved",
        ));
    }
    let written = async {
        file.write_all(text.as_bytes()).await?;
        file.flush().await
    }
    .await;
    if let Err(err) = written {
        drop(file);
        let _ = tokio::fs::remove_file(target).await;
        return Err(ToolError::new(format!(
            "cannot write `{}`: {err}",
            target.display()
        )));
    }
    Ok(text.len() as u64)
}

#[cfg(test)]
mod tests {
    use appstore_connect_api::executor::ClientConfig;

    use super::*;

    fn client() -> ApiClient {
        ApiClient::new(ClientConfig::new(None).unwrap()).unwrap()
    }

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            _ => panic!("не объект"),
        }
    }

    fn prepare_with(value: Value, access: AccessLevel) -> Result<Prepared, ToolError> {
        prepare(
            &args(value),
            &client(),
            access,
            &FilePolicy::new(&[]).unwrap(),
        )
    }

    #[test]
    fn relative_paths_and_query() {
        let prepared = prepare_with(
            json!({"path": "/v1/apps?limit=5&fields%5Bapps%5D=name", "query": {"filter.bundleId": ["a", "b"], "sort": "name"}}),
            AccessLevel::Read,
        )
        .unwrap();
        assert_eq!(prepared.request.method, HttpMethod::Get);
        assert_eq!(prepared.request.path, "/v1/apps");
        assert_eq!(
            prepared.request.query,
            vec![
                ("limit".to_owned(), "5".to_owned()),
                ("fields[apps]".to_owned(), "name".to_owned()),
                ("filter[bundleId]".to_owned(), "a,b".to_owned()),
                ("sort".to_owned(), "name".to_owned()),
            ]
        );
        assert!(
            matches!(prepared.matched, Some(RouteMatch::Operation(op)) if op.tool_name == "list_apps")
        );
    }

    #[test]
    fn rejects_foreign_and_unsafe_paths() {
        for path in [
            "https://example.com/v1/apps",
            "http://api.appstoreconnect.apple.com/v1/apps",
            "https://user:pw@api.appstoreconnect.apple.com/v1/apps",
            "/v1/../apps",
            "//example.com/v1/apps",
            "/v1/apps/%2e%2e/x",
            "apps",
            "/apps",
        ] {
            let err = prepare_with(json!({ "path": path }), AccessLevel::Destructive).unwrap_err();
            assert!(!err.message().is_empty(), "{path}");
        }
        // Адрес следующей страницы с хоста API принимается.
        let prepared = prepare_with(
            json!({"path": "https://api.appstoreconnect.apple.com/v1/apps?cursor=AB&limit=50"}),
            AccessLevel::Read,
        )
        .unwrap();
        assert_eq!(prepared.request.path, "/v1/apps");
        assert!(
            prepared
                .request
                .query
                .contains(&("cursor".into(), "AB".into()))
        );
        let err = prepare_with(
            json!({"path": "https://api.appstoreconnect.apple.com/v1/apps", "method": "POST"}),
            AccessLevel::Write,
        )
        .unwrap_err();
        assert!(err.message().contains("links.next"), "{err}");
        let err = prepare_with(
            json!({"path": "/v1/apps", "headers": {"Authorization": "x"}}),
            AccessLevel::Read,
        )
        .unwrap_err();
        assert!(err.message().contains("cannot be set"), "{err}");
    }

    #[test]
    fn access_comes_from_the_operation() {
        let err = prepare_with(
            json!({"method": "POST", "path": "/v1/betaGroups", "body": {"data": {}}}),
            AccessLevel::Read,
        )
        .unwrap_err();
        assert!(err.message().contains("`write` access"), "{err}");
        assert!(
            prepare_with(
                json!({"method": "POST", "path": "/v1/betaGroups", "body": {"data": {}}}),
                AccessLevel::Write,
            )
            .is_ok()
        );
        let err = prepare_with(
            json!({"method": "delete", "path": "/v1/betaGroups/1"}),
            AccessLevel::Write,
        )
        .unwrap_err();
        assert!(err.message().contains("`destructive` access"), "{err}");
        // Неизвестный путь — по методу.
        let err = prepare_with(
            json!({"method": "DELETE", "path": "/v1/unknownThings/1"}),
            AccessLevel::Write,
        )
        .unwrap_err();
        assert!(err.message().contains("`destructive` access"), "{err}");
        // Исключённая операция чтения доступна.
        assert!(
            prepare_with(
                json!({"path": "/v1/appCategories/GAMES/relationships/subcategories"}),
                AccessLevel::Read
            )
            .is_ok()
        );
    }

    #[test]
    fn bodies_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let files = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
        std::fs::write(
            root.join("body.json"),
            r#"{"data": {"type": "betaGroups"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("bad.json"), "[1]").unwrap();
        let prepare_files =
            |value: Value| prepare(&args(value), &client(), AccessLevel::Write, &files);

        let prepared = prepare_files(
            json!({"method": "POST", "path": "/v1/betaGroups", "body_file": "body.json"}),
        );
        let err = prepared.err();
        // Относительный путь без базового каталога политики отклоняется.
        assert!(err.is_some());
        let file = root.join("body.json").display().to_string();
        let prepared =
            prepare_files(json!({"method": "POST", "path": "/v1/betaGroups", "body_file": file}))
                .unwrap();
        assert!(
            matches!(prepared.request.body, RequestBody::Json(ref v) if v["data"]["type"] == "betaGroups")
        );

        let bad = root.join("bad.json").display().to_string();
        let err =
            prepare_files(json!({"method": "POST", "path": "/v1/betaGroups", "body_file": bad}))
                .unwrap_err();
        assert!(err.message().contains("JSON object"), "{err}");
        let err = prepare_files(
            json!({"method": "POST", "path": "/v1/betaGroups", "body": {}, "body_file": file}),
        )
        .unwrap_err();
        assert!(err.message().contains("not both"), "{err}");
        let err = prepare_files(json!({"path": "/v1/apps", "body": {}})).unwrap_err();
        assert!(
            err.message().contains("GET request cannot have a body"),
            "{err}"
        );
        let err = prepare_files(json!({"method": "POST", "path": "/v1/betaGroups", "body": [1]}))
            .unwrap_err();
        assert!(err.message().contains("JSON object"), "{err}");
    }

    #[test]
    fn binary_responses_need_save_to() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let files = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
        let path = json!("/v1/salesReports?filter[vendorNumber]=1&filter[reportType]=SALES");
        let err = prepare(
            &args(json!({ "path": path })),
            &client(),
            AccessLevel::Read,
            &files,
        )
        .unwrap_err();
        assert!(err.message().contains("save_to"), "{err}");
        let target = root.join("r.gz").display().to_string();
        let prepared = prepare(
            &args(json!({ "path": path, "save_to": target })),
            &client(),
            AccessLevel::Read,
            &files,
        )
        .unwrap();
        assert_eq!(prepared.request.response, ResponseKind::Binary);
        assert!(prepared.request.save_to.is_some());
        assert!(prepared.json_save_to.is_none());

        let prepared = prepare(
            &args(json!({ "path": "/v1/apps", "save_to": root.join("apps.json").display().to_string() })),
            &client(),
            AccessLevel::Read,
            &files,
        )
        .unwrap();
        assert!(prepared.request.save_to.is_none());
        assert_eq!(prepared.json_save_to, Some(root.join("apps.json")));
    }

    #[test]
    fn tool_annotations_follow_server_access() {
        for (access, read_only, destructive) in [
            (AccessLevel::Read, Some(true), None),
            (AccessLevel::Write, Some(false), Some(false)),
            (AccessLevel::Destructive, Some(false), Some(true)),
        ] {
            let tool = api_request_tool(access, true);
            let a = tool.annotations.unwrap();
            assert_eq!(
                (a.read_only_hint, a.destructive_hint),
                (read_only, destructive)
            );
            assert!(tool.input_schema["properties"].get("save_to").is_some());
            assert!(tool.input_schema["properties"].get("jq").is_some());
        }
        let tool = api_request_tool(AccessLevel::Read, false);
        assert!(tool.input_schema["properties"].get("body_file").is_none());
    }

    #[tokio::test]
    async fn json_files_are_created_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let target = root.join("out.json");
        assert_eq!(write_json_file(&target, "{}").await.unwrap(), 2);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{}");
        let err = write_json_file(&target, "{}").await.unwrap_err();
        assert!(err.message().contains("never overwritten"), "{err}");
    }
}
