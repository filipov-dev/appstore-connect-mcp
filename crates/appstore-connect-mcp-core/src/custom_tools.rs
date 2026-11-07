//! Пользовательские инструменты: операции App Store Connect API с фиксированными аргументами
//! и фильтром `jq` из `tools.toml`.
//!
//! Файл читает бинарник (`serve`, `tools check`) и передаёт разобранные [`CustomToolDef`]
//! сюда: [`CustomTool::compile`] проверяет определение. Инструмент — это одна операция из
//! таблицы операций (`operation = "list_builds"`), поэтому у него те же границы, что у
//! инструмента этой операции:
//!
//! - уровень доступа — уровень операции; инструмент выше уровня сервера не регистрируется
//!   (вызов объясняет, какой флаг нужен), наборы инструментов (`--toolsets`) его не
//!   ограничивают;
//! - запрос собирает тот же [`crate::request::build_request`]: неизвестные аргументы
//!   отклоняются, файлы проходят [`crate::FilePolicy`], ответ проходит ту же постобработку;
//! - фиксированные аргументы (`args`) проверяются при загрузке файла, из схемы инструмента
//!   убираются, а переданный агентом фиксированный аргумент — ошибка: агент не может
//!   подменить то, что задал пользователь. `save_to` фиксировать нельзя;
//! - фильтр `jq` определения применяется к ответу; фильтр из вызова агента применяется
//!   к его результату (`(фильтр файла) | (фильтр вызова)`).
//!
//! Имя не должно совпадать со встроенными инструментами (всеми операциями, а не только
//! зарегистрированными, `api_request`, `api_search`, инструментами динамических наборов и
//! составными инструментами) и с другими пользовательскими инструментами: смысл файла не
//! зависит от флагов сервера.

use std::collections::HashSet;
use std::sync::Arc;

use appstore_connect_api::spec::{AccessLevel, OperationDef, find, operations};
use rmcp::model::{JsonObject, Tool};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::api_request::API_REQUEST_TOOL;
use crate::api_search::API_SEARCH_TOOL;
use crate::composite::CompositeTool;
use crate::dynamic_toolsets::{DISABLE_TOOLSET_TOOL, ENABLE_TOOLSET_TOOL, LIST_TOOLSETS_TOOL};
use crate::error::ToolError;
use crate::jq::{JQ_ARG, JqFilter, take_jq, takes_jq};
use crate::registry::{MAX_TOOL_NAME_LEN, operation_tool};
use crate::request::check_fixed_argument;

/// Наибольшая длина значения фиксированного аргумента в описании инструмента.
const MAX_SHOWN_VALUE: usize = 200;

/// Содержимое файла `tools.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomToolsFile {
    #[serde(default)]
    pub tools: Vec<CustomToolDef>,
}

/// Определение инструмента в файле.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomToolDef {
    /// Имя MCP-инструмента.
    pub name: String,
    /// Описание для агента: что делает инструмент и когда его вызывать.
    pub description: String,
    /// Имя операции (инструмента операции), например `list_builds`.
    pub operation: String,
    /// Фиксированные аргументы операции: те же имена, что у инструмента операции
    /// (`id`, `filter.app`, `limit`, `body`, ...).
    #[serde(default)]
    pub args: Map<String, Value>,
    /// Фильтр ответа (см. [`crate::jq`]).
    #[serde(default)]
    pub jq: Option<String>,
}

/// Проверенный пользовательский инструмент.
#[derive(Debug, Clone)]
pub struct CustomTool {
    pub name: String,
    pub description: String,
    pub operation: &'static OperationDef,
    fixed: JsonObject,
    jq: Option<JqFilter>,
    /// Откуда определение (путь к файлу) — для сообщений об ошибках и `tools list`.
    pub source: String,
}

impl CustomTool {
    /// Проверяет определение. Ошибка называет файл и инструмент.
    pub fn compile(def: CustomToolDef, source: &str) -> Result<Self, String> {
        let name = def.name.trim().to_owned();
        check_identifier(&name).map_err(|err| format!("{source}: {err}"))?;
        let fail = |message: String| format!("{source}: tool `{name}`: {message}");
        let description = def.description.trim().to_owned();
        if description.is_empty() {
            return Err(fail("`description` is empty".into()));
        }
        let operation_name = def.operation.trim();
        let Some(operation) = find(operation_name) else {
            return Err(fail(format!(
                "unknown operation `{operation_name}`; use a tool name of an App Store Connect \
                 API operation, e.g. `list_builds` (see `appstore-connect-mcp toolsets` or \
                 `api_search`)"
            )));
        };
        let schema: JsonObject = serde_json::from_str(operation.input_schema)
            .map_err(|err| fail(format!("invalid operation schema: {err}")))?;
        for (arg, value) in &def.args {
            if arg == JQ_ARG {
                return Err(fail(format!(
                    "`{JQ_ARG}` is not an operation argument; set the filter with the `jq` key \
                     of the tool"
                )));
            }
            check_fixed_argument(operation, &schema, arg, value)
                .map_err(|err| fail(format!("`args`: {}", err.message())))?;
        }
        let jq = match def.jq.as_deref() {
            Some(_) if !takes_jq(operation) => {
                return Err(fail(format!(
                    "`jq` is not supported for `{}`: it downloads a file, not JSON",
                    operation.tool_name
                )));
            }
            Some(code) => Some(
                JqFilter::parse(code).map_err(|err| fail(format!("`jq`: {}", err.message())))?,
            ),
            None => None,
        };
        Ok(Self {
            name,
            description,
            operation,
            fixed: def.args,
            jq,
            source: source.to_owned(),
        })
    }

    /// Уровень доступа инструмента — уровень его операции.
    pub fn access(&self) -> AccessLevel {
        self.operation.access
    }

    /// Фиксированные аргументы.
    pub fn fixed_args(&self) -> &JsonObject {
        &self.fixed
    }

    /// Фильтр ответа из определения.
    pub fn jq(&self) -> Option<&JqFilter> {
        self.jq.as_ref()
    }

    /// Описание MCP-инструмента: схема операции без фиксированных аргументов, аннотации —
    /// по уровню операции.
    pub fn tool(&self) -> Result<Tool, String> {
        let op = self.operation;
        let base = operation_tool(op)?;
        let mut schema = (*base.input_schema).clone();
        if let Some(Value::Object(properties)) = schema.get_mut("properties") {
            for arg in self.fixed.keys() {
                properties.remove(arg);
            }
        }
        let mut drop_required = false;
        if let Some(Value::Array(required)) = schema.get_mut("required") {
            required.retain(|item| {
                item.as_str()
                    .is_none_or(|name| !self.fixed.contains_key(name))
            });
            drop_required = required.is_empty();
        }
        if drop_required {
            schema.remove("required");
        }

        let mut description = format!(
            "{}\n\nCustom tool defined by the user: calls `{}` ({} {})",
            self.description,
            op.tool_name,
            op.method.as_str(),
            op.path
        );
        if !self.fixed.is_empty() {
            let fixed: Vec<String> = self
                .fixed
                .iter()
                .map(|(name, value)| format!("`{name}` = {}", shown_value(value)))
                .collect();
            description.push_str(&format!(
                " with fixed arguments {} (they cannot be passed)",
                fixed.join(", ")
            ));
        }
        if let Some(jq) = &self.jq {
            description.push_str(&format!(
                "; the response is filtered with `{}` before your own `{JQ_ARG}` filter",
                jq.as_str()
            ));
        }
        description.push('.');
        description.push_str("\n\nAccess: ");
        description.push_str(match op.access {
            AccessLevel::Read => "read-only.",
            AccessLevel::Write => "write (creates or changes data).",
            AccessLevel::Destructive => "destructive (deletes data or makes irreversible changes).",
        });

        let mut tool = Tool::new(self.name.clone(), description, Arc::new(schema));
        tool.annotations = base.annotations.map(|mut annotations| {
            annotations.title = None;
            annotations
        });
        Ok(tool)
    }

    /// Аргументы операции для вызова (аргументы агента плюс фиксированные) и итоговый
    /// фильтр: фильтр определения, затем фильтр из вызова.
    pub fn build_call(
        &self,
        mut args: JsonObject,
    ) -> Result<(JsonObject, Option<JqFilter>), ToolError> {
        // У операций с двоичным ответом аргумента `jq` нет: его отклонит `build_request`.
        let call_jq = if takes_jq(self.operation) {
            take_jq(&mut args)?
        } else {
            None
        };
        let jq = match (&self.jq, call_jq) {
            (Some(own), Some(extra)) => Some(JqFilter::parse(&format!(
                "({}) | ({})",
                own.as_str(),
                extra.as_str()
            ))?),
            (own, extra) => extra.or_else(|| own.clone()),
        };
        let overridden: Vec<String> = args
            .iter()
            .filter(|(key, value)| !value.is_null() && self.fixed.contains_key(key.as_str()))
            .map(|(key, _)| format!("`{key}`"))
            .collect();
        if !overridden.is_empty() {
            return Err(ToolError::new(format!(
                "argument(s) {} are fixed by the definition of custom tool `{}` and cannot be \
                 passed; use the `{}` tool to set them yourself",
                overridden.join(", "),
                self.name,
                self.operation.tool_name
            )));
        }
        for (key, value) in &self.fixed {
            args.insert(key.clone(), value.clone());
        }
        Ok((args, jq))
    }
}

/// Проверяет набор пользовательских инструментов: имена уникальны и не совпадают со
/// встроенными и составными. Встроенные имена берутся из всей таблицы операций, а не только
/// из зарегистрированных, чтобы смысл файла не зависел от флагов доступа и наборов.
pub fn check_names(tools: &[CustomTool], composite: &[CompositeTool]) -> Result<(), String> {
    let builtin: HashSet<&str> = operations()
        .iter()
        .map(|op| op.tool_name)
        .chain([
            API_REQUEST_TOOL,
            API_SEARCH_TOOL,
            LIST_TOOLSETS_TOOL,
            ENABLE_TOOLSET_TOOL,
            DISABLE_TOOLSET_TOOL,
        ])
        .chain(composite.iter().map(|tool| tool.name))
        .collect();
    let mut seen: HashSet<&str> = HashSet::new();
    for tool in tools {
        if builtin.contains(tool.name.as_str()) {
            return Err(format!(
                "{}: tool `{}` has the same name as a built-in tool; rename it",
                tool.source, tool.name
            ));
        }
        if !seen.insert(tool.name.as_str()) {
            return Err(format!(
                "{}: tool `{}` is defined more than once",
                tool.source, tool.name
            ));
        }
    }
    Ok(())
}

fn check_identifier(name: &str) -> Result<(), String> {
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= MAX_TOOL_NAME_LEN;
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid tool name `{name}`: use lowercase letters, digits and `_`, starting with a \
             letter, at most {MAX_TOOL_NAME_LEN} characters"
        ))
    }
}

/// Значение фиксированного аргумента для описания: компактный JSON, длинное — с многоточием.
fn shown_value(value: &Value) -> String {
    let mut text = value.to_string();
    if crate::jq::truncate_text(&mut text, MAX_SHOWN_VALUE) {
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn compile(value: Value) -> Result<CustomTool, String> {
        CustomTool::compile(serde_json::from_value(value).unwrap(), "tools.toml")
    }

    fn recent_builds() -> Value {
        json!({
            "name": "recent_builds",
            "description": "Latest builds of the main app",
            "operation": "list_builds",
            "args": {
                "filter.app": ["1234567890"],
                "sort": ["-uploadedDate"],
                "limit": 10
            },
            "jq": "[.data[] | {id, version: .attributes.version}]"
        })
    }

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            _ => panic!("не объект"),
        }
    }

    #[test]
    fn compiles_and_describes() {
        let tool = compile(recent_builds()).unwrap();
        assert_eq!(tool.access(), AccessLevel::Read);
        let mcp = tool.tool().unwrap();
        assert_eq!(mcp.name, "recent_builds");
        let properties = mcp.input_schema["properties"].as_object().unwrap();
        // Фиксированные аргументы убраны, остальные аргументы операции и `jq` остались.
        assert!(!properties.contains_key("filter.app"));
        assert!(!properties.contains_key("limit"));
        assert!(properties.contains_key("fields.builds"));
        assert!(properties.contains_key(JQ_ARG));
        assert_eq!(mcp.input_schema["additionalProperties"], false);
        let description = mcp.description.as_deref().unwrap();
        assert!(description.starts_with("Latest builds of the main app"));
        assert!(
            description.contains("`list_builds` (GET /v1/builds)"),
            "{description}"
        );
        assert!(description.contains("`limit` = 10"), "{description}");
        assert!(description.contains("Access: read-only."));
        let annotations = mcp.annotations.unwrap();
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.title, None);
    }

    #[test]
    fn required_fixed_arguments_leave_the_schema() {
        let tool = compile(json!({
            "name": "main_app",
            "description": "The main app",
            "operation": "get_app",
            "args": {"id": "1234567890"}
        }))
        .unwrap();
        let mcp = tool.tool().unwrap();
        assert!(mcp.input_schema.get("required").is_none());
        let (request, jq) = tool.build_call(JsonObject::new()).unwrap();
        assert_eq!(request["id"], "1234567890");
        assert!(jq.is_none());
    }

    #[test]
    fn builds_calls() {
        let tool = compile(recent_builds()).unwrap();
        let (request, jq) = tool
            .build_call(args(json!({"fields.builds": ["version"]})))
            .unwrap();
        assert_eq!(request["limit"], 10);
        assert_eq!(request["fields.builds"], json!(["version"]));
        assert_eq!(
            jq.unwrap().as_str(),
            "[.data[] | {id, version: .attributes.version}]"
        );

        let (_, jq) = tool.build_call(args(json!({"jq": ".[0]"}))).unwrap();
        assert_eq!(
            jq.unwrap().as_str(),
            "([.data[] | {id, version: .attributes.version}]) | (.[0])"
        );

        let err = tool.build_call(args(json!({"limit": 200}))).unwrap_err();
        assert!(err.message().contains("`limit` are fixed"), "{err}");
        assert!(tool.build_call(args(json!({"limit": null}))).is_ok());
        let err = tool.build_call(args(json!({"jq": ".a |"}))).unwrap_err();
        assert!(err.message().contains("argument `jq`"), "{err}");
    }

    #[test]
    fn access_comes_from_the_operation() {
        let create = compile(json!({
            "name": "create_qa_group",
            "description": "Create a QA group",
            "operation": "create_beta_group"
        }))
        .unwrap();
        assert_eq!(create.access(), AccessLevel::Write);
        let annotations = create.tool().unwrap().annotations.unwrap();
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.destructive_hint, Some(false));
        let delete = operations()
            .iter()
            .find(|op| op.access == AccessLevel::Destructive)
            .unwrap();
        let tool = compile(json!({
            "name": "drop_it",
            "description": "Delete",
            "operation": delete.tool_name
        }))
        .unwrap();
        assert_eq!(tool.access(), AccessLevel::Destructive);
    }

    #[test]
    fn rejects_invalid_definitions() {
        let cases = [
            (json!({"name": "Bad-Name"}), "invalid tool name"),
            (json!({"description": " "}), "`description` is empty"),
            (
                json!({"operation": "no_such_op"}),
                "unknown operation `no_such_op`",
            ),
            (json!({"args": {"nope": 1}}), "has no argument `nope`"),
            (json!({"args": {"limit": "many"}}), "must be an integer"),
            (json!({"args": {"limit": null}}), "cannot be fixed to null"),
            (
                json!({"args": {"filter.processingState": ["NOPE"]}}),
                "not an allowed value",
            ),
            (json!({"args": {"jq": "."}}), "not an operation argument"),
            (json!({"jq": ".a |"}), "`jq`: argument `jq`: syntax error"),
            (json!({"jq": "env"}), "undefined filter `env/0`"),
        ];
        for (patch, needle) in cases {
            let mut value = recent_builds();
            for (key, item) in patch.as_object().unwrap() {
                value[key] = item.clone();
            }
            let err = compile(value).unwrap_err();
            assert!(err.contains(needle), "{needle}: {err}");
            assert!(err.starts_with("tools.toml: "), "{err}");
        }

        let download = json!({
            "name": "daily_sales",
            "description": "Daily sales report",
            "operation": "list_sales_reports",
        });
        let mut with_jq = download.clone();
        with_jq["jq"] = json!(".");
        assert!(compile(with_jq).unwrap_err().contains("downloads a file"));
        let mut fixed_target = download.clone();
        fixed_target["args"] = json!({"save_to": "/tmp/x"});
        assert!(
            compile(fixed_target)
                .unwrap_err()
                .contains("cannot be fixed")
        );
        let tool = compile(download).unwrap();
        let schema = tool.tool().unwrap().input_schema;
        assert!(schema["properties"].get("save_to").is_some());
        assert!(schema["properties"].get(JQ_ARG).is_none());

        let unknown_key = serde_json::from_value::<CustomToolDef>(json!({
            "name": "x", "description": "d", "operation": "get_app", "method": "GET"
        }));
        assert!(unknown_key.is_err());
    }

    #[test]
    fn names_must_not_clash() {
        let mut builtin = recent_builds();
        builtin["name"] = json!("get_app");
        let err = check_names(&[compile(builtin).unwrap()], &[]).unwrap_err();
        assert!(err.contains("built-in"), "{err}");
        for name in [API_REQUEST_TOOL, API_SEARCH_TOOL, ENABLE_TOOLSET_TOOL] {
            let mut builtin = recent_builds();
            builtin["name"] = json!(name);
            assert!(
                check_names(&[compile(builtin).unwrap()], &[]).is_err(),
                "{name}"
            );
        }
        let twice = [
            compile(recent_builds()).unwrap(),
            compile(recent_builds()).unwrap(),
        ];
        assert!(
            check_names(&twice, &[])
                .unwrap_err()
                .contains("more than once")
        );
        assert!(check_names(&[compile(recent_builds()).unwrap()], &[]).is_ok());

        fn run(_: &crate::composite::Context, _: Value) -> crate::composite::ToolFuture<'_> {
            Box::pin(async { Ok(Value::Null) })
        }
        let composite = CompositeTool {
            name: "recent_builds",
            toolset: "builds",
            access: AccessLevel::Read,
            description: "x",
            input_schema: || json!({"type": "object"}),
            uses_local_files: false,
            run,
        };
        let err = check_names(&[compile(recent_builds()).unwrap()], &[composite]).unwrap_err();
        assert!(err.contains("built-in"), "{err}");
    }
}
