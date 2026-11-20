//! Реестр MCP-инструментов сервера.
//!
//! Строится при запуске из `appstore_connect_api::spec::operations()` и составных
//! инструментов ([`ServerOptions::composite_tools`]) с учётом [`ServerOptions`]: инструменты
//! выше `max_access` и вне включённых наборов не регистрируются, поэтому не видны в
//! `tools/list` и не вызываются. Для незарегистрированных, но известных инструментов
//! [`ToolRegistry::unavailable_reason`] объясняет, какой флаг их включает.
//!
//! Без [`ServerOptions::local_files`] не регистрируются операции с двоичным ответом (их
//! результат сохраняется в файл) и составные инструменты, работающие с файлами.
//!
//! Схема аргументов инструмента операции — `input_schema` операции и добавления ядра:
//! обязательный `save_to` у операций с двоичным ответом, фильтр `jq` ([`crate::jq`]) у
//! операций с JSON-ответом и необязательный `flatten` у операций с `include`. Составные
//! инструменты тоже получают `jq`.
//!
//! Кроме операций, всегда (независимо от `--toolsets`) регистрируются `api_search` (поиск по
//! всем операциям, только чтение) и `api_request` (запрос к любому пути API в пределах уровня
//! доступа сервера; аннотации — по максимальному уровню).
//!
//! Пользовательские инструменты ([`crate::custom_tools`]) регистрируются по уровню доступа
//! своей операции и локальным файлам; наборы их не ограничивают.
//!
//! В режиме динамических наборов ([`ServerOptions::dynamic_toolsets`]) реестр — снимок для
//! текущего списка включённых наборов: сервер пересобирает его через
//! [`ToolRegistry::with_toolsets`] при `enable_toolset`/`disable_toolset`. Пустой список
//! наборов в этом режиме означает «ни одного», а не набор по умолчанию.

use std::collections::HashMap;
use std::sync::Arc;

use appstore_connect_api::spec::{
    AccessLevel, HttpMethod, OperationDef, find, find_toolset, operations,
};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};

use crate::ServerOptions;
use crate::api_request::{API_REQUEST_TOOL, api_request_tool};
use crate::api_search::{API_SEARCH_TOOL, api_search_tool};
use crate::composite::CompositeTool;
use crate::custom_tools::check_names;
use crate::dynamic_toolsets::{
    DISABLE_TOOLSET_TOOL, ENABLE_TOOLSET_TOOL, LIST_TOOLSETS_TOOL, disable_toolset_tool,
    enable_toolset_tool, list_toolsets_tool,
};
use crate::jq::{JQ_ARG, add_jq_argument, takes_jq};
use crate::request::{FLATTEN_ARG, SAVE_TO_ARG, takes_flatten, takes_save_to};
use crate::toolsets::{effective_toolsets, normalize_toolsets};

/// Максимальная длина имени инструмента: некоторые клиенты добавляют к имени префикс
/// сервера и ограничивают итог 64 символами.
pub const MAX_TOOL_NAME_LEN: usize = 40;

/// Что выполняет инструмент.
#[derive(Debug, Clone, Copy)]
pub enum ToolKind {
    /// Сгенерированная операция App Store Connect API.
    Operation(&'static OperationDef),
    /// Составной инструмент: индекс в `ServerOptions::composite_tools`.
    Composite(usize),
    /// `api_request`: запрос к произвольному пути API.
    ApiRequest,
    /// `api_search`: поиск по таблице операций, без обращения к API.
    ApiSearch,
    /// Пользовательский инструмент: индекс в `ServerOptions::custom_tools`.
    Custom(usize),
    /// `list_toolsets` (динамические наборы).
    ListToolsets,
    /// `enable_toolset` (динамические наборы).
    EnableToolset,
    /// `disable_toolset` (динамические наборы).
    DisableToolset,
}

/// Зарегистрированный инструмент: описание для `tools/list` и обработчик.
#[derive(Debug, Clone)]
pub struct RegisteredTool {
    pub tool: Tool,
    pub kind: ToolKind,
    /// Уровень доступа инструмента; у `api_request` — максимальный уровень сервера.
    pub access: AccessLevel,
}

/// Набор инструментов, доступных при заданных [`ServerOptions`].
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    index: HashMap<String, usize>,
    /// Включённые наборы (slug).
    toolsets: Vec<String>,
    max_access: AccessLevel,
    /// Инструменты могут работать с локальными файлами ([`ServerOptions::local_files`]).
    local_files: bool,
    composite: Vec<CompositeTool>,
    /// Режим динамических наборов ([`ServerOptions::dynamic_toolsets`]).
    dynamic: bool,
    /// Пользовательские инструменты, доступные при уровне доступа сервера: не зависят от
    /// наборов, при пересборке реестра (`with_toolsets`) переносятся как есть.
    custom: Vec<RegisteredTool>,
    /// Пользовательские инструменты, которые не зарегистрированы, и почему.
    disabled_custom: HashMap<String, String>,
}

/// Работает ли инструмент операции с локальными файлами: сохранение двоичного ответа.
pub fn uses_local_files(op: &OperationDef) -> bool {
    takes_save_to(op)
}

impl ToolRegistry {
    /// Строит реестр. Ошибка — неизвестный набор инструментов, конфликт имён составных
    /// инструментов или некорректная схема (последнее — ошибка генератора или инструмента).
    pub fn new(options: &ServerOptions) -> Result<Self, String> {
        let selected = match &options.toolsets {
            Some(names) => Some(normalize_toolsets(names)?),
            None => None,
        };
        let toolsets = if options.dynamic_toolsets {
            selected.unwrap_or_default()
        } else {
            effective_toolsets(selected.as_deref())
        };
        check_composite(&options.composite_tools)?;
        check_names(&options.custom_tools, &options.composite_tools)?;
        let mut custom = Vec::new();
        let mut disabled_custom = HashMap::new();
        for (index, tool) in options.custom_tools.iter().enumerate() {
            let op = tool.operation;
            if !options.local_files && uses_local_files(op) {
                disabled_custom.insert(
                    tool.name.clone(),
                    format!(
                        "custom tool `{}` is not available on this server: its operation `{}` \
                         writes a local file, and local files are disabled",
                        tool.name, op.tool_name
                    ),
                );
            } else if !options.max_access.allows(op.access) {
                disabled_custom.insert(
                    tool.name.clone(),
                    format!(
                        "custom tool `{}` is disabled: it {} data in App Store Connect and the \
                         server runs with {} access. Ask the user to {} if this is intended",
                        tool.name,
                        access_verb(op.access),
                        access_name(options.max_access),
                        access_hint(op.access),
                    ),
                );
            } else {
                custom.push(RegisteredTool {
                    tool: tool.tool()?,
                    kind: ToolKind::Custom(index),
                    access: op.access,
                });
            }
        }
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            toolsets,
            max_access: options.max_access,
            local_files: options.local_files,
            composite: options.composite_tools.clone(),
            dynamic: options.dynamic_toolsets,
            custom,
            disabled_custom,
        }
        .populate()
    }

    /// Тот же реестр с другим списком включённых наборов (динамический режим).
    /// Ошибка — неизвестный набор.
    pub fn with_toolsets(&self, toolsets: &[String]) -> Result<Self, String> {
        let toolsets = normalize_toolsets(toolsets)?;
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            toolsets: if self.dynamic {
                toolsets
            } else {
                effective_toolsets(Some(&toolsets))
            },
            max_access: self.max_access,
            local_files: self.local_files,
            composite: self.composite.clone(),
            dynamic: self.dynamic,
            custom: self.custom.clone(),
            disabled_custom: self.disabled_custom.clone(),
        }
        .populate()
    }

    /// Заполняет пустой реестр инструментами по его настройкам.
    fn populate(self) -> Result<Self, String> {
        let mut registry = self;
        for op in operations() {
            if registry.operation_enabled(op) {
                let tool = operation_tool(op)?;
                registry.push(RegisteredTool {
                    tool,
                    kind: ToolKind::Operation(op),
                    access: op.access,
                });
            }
        }
        let max_access = registry.max_access;
        registry.push(RegisteredTool {
            tool: api_request_tool(max_access, registry.local_files),
            kind: ToolKind::ApiRequest,
            access: max_access,
        });
        registry.push(RegisteredTool {
            tool: api_search_tool(),
            kind: ToolKind::ApiSearch,
            access: AccessLevel::Read,
        });
        for (index, composite) in registry.composite.clone().iter().enumerate() {
            if registry.composite_enabled(composite) {
                registry.push(RegisteredTool {
                    tool: composite_tool(composite)?,
                    kind: ToolKind::Composite(index),
                    access: composite.access,
                });
            }
        }
        if registry.dynamic {
            // Управление наборами меняет только список инструментов этого сервера, данных
            // App Store Connect не касается — уровень чтения.
            for (tool, kind) in [
                (list_toolsets_tool(), ToolKind::ListToolsets),
                (enable_toolset_tool(), ToolKind::EnableToolset),
                (disable_toolset_tool(), ToolKind::DisableToolset),
            ] {
                registry.push(RegisteredTool {
                    tool,
                    kind,
                    access: AccessLevel::Read,
                });
            }
        }
        // Пользовательские инструменты пользователь настроил явно: они регистрируются всегда,
        // в том числе в динамическом режиме, и не входят ни в один набор.
        for tool in registry.custom.clone() {
            registry.push(tool);
        }
        Ok(registry)
    }

    fn operation_enabled(&self, op: &OperationDef) -> bool {
        self.max_access.allows(op.access)
            && self.toolset_enabled(op.toolset)
            && (self.local_files || !uses_local_files(op))
    }

    fn composite_enabled(&self, tool: &CompositeTool) -> bool {
        self.max_access.allows(tool.access)
            && self.toolset_enabled(tool.toolset)
            && (self.local_files || !tool.uses_local_files)
    }

    /// Включён ли набор `slug`.
    pub fn toolset_enabled(&self, slug: &str) -> bool {
        self.toolsets.iter().any(|name| name == slug)
    }

    /// Включённые наборы (slug).
    pub fn enabled_toolsets(&self) -> &[String] {
        &self.toolsets
    }

    /// Максимальный уровень доступа сервера.
    pub fn max_access(&self) -> AccessLevel {
        self.max_access
    }

    /// Могут ли инструменты работать с локальными файлами.
    pub fn local_files(&self) -> bool {
        self.local_files
    }

    /// Режим динамических наборов: наборы включаются инструментом `enable_toolset`.
    pub fn is_dynamic(&self) -> bool {
        self.dynamic
    }

    /// Составные инструменты сервера (зарегистрированные и нет).
    pub fn composite_tools(&self) -> &[CompositeTool] {
        &self.composite
    }

    fn push(&mut self, tool: RegisteredTool) {
        self.index
            .insert(tool.tool.name.to_string(), self.tools.len());
        self.tools.push(tool);
    }

    /// Зарегистрированные инструменты в порядке регистрации: операции по имени, затем
    /// `api_request`, `api_search`, составные, инструменты динамических наборов и
    /// пользовательские.
    pub fn iter(&self) -> impl Iterator<Item = &RegisteredTool> {
        self.tools.iter()
    }

    /// Описания инструментов для `tools/list`.
    pub fn tools(&self) -> Vec<Tool> {
        self.tools.iter().map(|t| t.tool.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&RegisteredTool> {
        self.index.get(name).and_then(|&i| self.tools.get(i))
    }

    /// Почему известный инструмент не зарегистрирован; `None` — инструмент неизвестен или
    /// зарегистрирован.
    pub fn unavailable_reason(&self, name: &str) -> Option<String> {
        if self.get(name).is_some() {
            return None;
        }
        if let Some(reason) = self.disabled_custom.get(name) {
            return Some(reason.clone());
        }
        let (access, toolset, files, call) = if let Some(op) = find(name) {
            (
                op.access,
                op.toolset,
                uses_local_files(op),
                Some(format!("{} {}", op.method.as_str(), op.path)),
            )
        } else {
            let tool = self.composite.iter().find(|tool| tool.name == name)?;
            (tool.access, tool.toolset, tool.uses_local_files, None)
        };
        if !self.local_files && files {
            return Some(format!(
                "tool `{name}` is not available on this server: it reads or writes local files, \
                 and local files are disabled"
            ));
        }
        if !self.max_access.allows(access) {
            return Some(format!(
                "tool `{name}` is disabled: it {} data in App Store Connect and the server runs \
                 with {} access. Ask the user to {} if this change is intended",
                access_verb(access),
                access_name(self.max_access),
                access_hint(access),
            ));
        }
        if !self.toolset_enabled(toolset) {
            let alternative = call
                .map(|call| format!(", or call the same operation with `api_request` ({call})"))
                .unwrap_or_default();
            if self.dynamic {
                return Some(format!(
                    "tool `{name}` belongs to toolset `{toolset}`, which is not enabled: call \
                     `{ENABLE_TOOLSET_TOOL}` with `toolset: \"{toolset}\"` first{alternative}"
                ));
            }
            return Some(format!(
                "tool `{name}` belongs to toolset `{toolset}`, which is not enabled; ask the user \
                 to add it to --toolsets (enabled: {}){alternative}",
                self.toolsets.join(", ")
            ));
        }
        None
    }
}

/// Проверяет составные инструменты: имена уникальны, не совпадают с операциями, не длиннее
/// [`MAX_TOOL_NAME_LEN`], набор существует.
fn check_composite(tools: &[CompositeTool]) -> Result<(), String> {
    const BUILT_IN: [&str; 5] = [
        API_REQUEST_TOOL,
        API_SEARCH_TOOL,
        LIST_TOOLSETS_TOOL,
        ENABLE_TOOLSET_TOOL,
        DISABLE_TOOLSET_TOOL,
    ];
    for name in BUILT_IN {
        if find(name).is_some() {
            return Err(format!(
                "operation `{name}` clashes with the built-in tool of the same name"
            ));
        }
    }
    let mut seen: Vec<&str> = BUILT_IN.to_vec();
    for tool in tools {
        if seen.contains(&tool.name) {
            return Err(format!(
                "composite tool `{}` is defined twice or clashes with a built-in tool",
                tool.name
            ));
        }
        if find(tool.name).is_some() {
            return Err(format!(
                "composite tool `{}` clashes with the operation tool of the same name",
                tool.name
            ));
        }
        if tool.name.len() > MAX_TOOL_NAME_LEN || tool.name.is_empty() {
            return Err(format!(
                "composite tool name `{}` must be 1-{MAX_TOOL_NAME_LEN} characters long",
                tool.name
            ));
        }
        if find_toolset(tool.toolset).is_none() {
            return Err(format!(
                "composite tool `{}` belongs to an unknown toolset `{}`",
                tool.name, tool.toolset
            ));
        }
        seen.push(tool.name);
    }
    Ok(())
}

/// Описание MCP-инструмента для операции.
///
/// Схема — `input_schema` операции плюс добавления ядра (`save_to`, `flatten`), описание —
/// `summary`, `description`, пояснения к добавлениям и уровень доступа.
pub fn operation_tool(op: &'static OperationDef) -> Result<Tool, String> {
    let mut schema: JsonObject = serde_json::from_str(op.input_schema)
        .map_err(|err| format!("{}: invalid input schema: {err}", op.tool_name))?;
    if takes_save_to(op) {
        add_property(
            op.tool_name,
            &mut schema,
            SAVE_TO_ARG,
            json!({
                "type": "string",
                "description": "Path of a new local file to save the downloaded content to \
                    (absolute, or relative to the server's working directory). The directory must \
                    exist and be inside the directories allowed by the server; existing files are \
                    never overwritten."
            }),
            true,
        )?;
    }
    if takes_flatten(op) {
        add_property(
            op.tool_name,
            &mut schema,
            FLATTEN_ARG,
            flatten_schema(),
            false,
        )?;
    }
    if takes_jq(op) {
        add_jq(op.tool_name, &mut schema)?;
    }

    let summary = op.summary.trim();
    let mut description = summary.to_owned();
    let details = op.description.trim();
    if !details.is_empty() && details != summary {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str(details);
    }
    if takes_save_to(op) {
        description.push_str(
            "\n\nThe response is a file (usually gzip-compressed): it is saved to `save_to` (a \
             new file inside the allowed directories) and the result reports its path and size.",
        );
    }
    description.push_str("\n\nAccess: ");
    description.push_str(access_description(op.access));

    let mut tool = Tool::new(op.tool_name, description, Arc::new(schema));
    if !summary.is_empty() {
        tool.title = Some(summary.to_owned());
    }
    let idempotent = matches!(op.method, HttpMethod::Put | HttpMethod::Delete);
    tool.annotations = Some(annotations(summary, op.access, Some(idempotent)));
    Ok(tool)
}

/// Схема аргумента `flatten`.
pub fn flatten_schema() -> Value {
    json!({
        "type": "boolean",
        "description": "Return each resource as a flat object `{id, type, ...attributes, \
            <relationship>: <related object or list>}`: related resources requested with \
            `include` replace their `{type, id}` references, and `included` and `links` are \
            dropped (pagination is still reported). Default false."
    })
}

/// Описание MCP-инструмента для составного инструмента.
pub fn composite_tool(composite: &CompositeTool) -> Result<Tool, String> {
    let Value::Object(schema) = (composite.input_schema)() else {
        return Err(format!(
            "composite tool `{}`: the input schema is not a JSON object",
            composite.name
        ));
    };
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(format!(
            "composite tool `{}`: the input schema must have `type: object`",
            composite.name
        ));
    }
    let mut schema = schema;
    add_jq(composite.name, &mut schema)?;
    let mut description = composite.description.trim().to_owned();
    description.push_str("\n\nAccess: ");
    description.push_str(access_description(composite.access));
    let mut tool = Tool::new(composite.name, description, Arc::new(schema));
    tool.annotations = Some(annotations("", composite.access, None));
    Ok(tool)
}

fn access_description(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read-only.",
        AccessLevel::Write => "write (creates or changes data).",
        AccessLevel::Destructive => "destructive (deletes data or makes irreversible changes).",
    }
}

/// Аннотации по уровню доступа. `idempotentHint` имеет смысл только для изменяющих
/// инструментов: `PUT` и `DELETE` идемпотентны, `POST` и `PATCH` — нет.
pub fn annotations(title: &str, access: AccessLevel, idempotent: Option<bool>) -> ToolAnnotations {
    let base = if title.is_empty() {
        ToolAnnotations::new()
    } else {
        ToolAnnotations::with_title(title)
    }
    .open_world(true);
    let changing = |base: ToolAnnotations, destructive: bool| {
        let base = base.read_only(false).destructive(destructive);
        match idempotent {
            Some(idempotent) => base.idempotent(idempotent),
            None => base,
        }
    };
    match access {
        AccessLevel::Read => base.read_only(true),
        AccessLevel::Write => changing(base, false),
        AccessLevel::Destructive => changing(base, true),
    }
}

/// Добавляет аргумент `jq`; ошибка — у инструмента уже есть свойство с этим именем.
fn add_jq(tool: &str, schema: &mut JsonObject) -> Result<(), String> {
    if schema
        .get("properties")
        .and_then(|properties| properties.get(JQ_ARG))
        .is_some()
    {
        return Err(format!(
            "{tool}: parameter `{JQ_ARG}` clashes with the response filter argument"
        ));
    }
    schema
        .entry("properties")
        .or_insert_with(|| Value::Object(JsonObject::new()));
    add_jq_argument(schema);
    Ok(())
}

/// Добавляет свойство в схему инструмента.
pub(crate) fn add_property(
    tool: &str,
    schema: &mut JsonObject,
    name: &str,
    property: Value,
    required: bool,
) -> Result<(), String> {
    let properties = schema
        .entry("properties")
        .or_insert_with(|| Value::Object(JsonObject::new()));
    let Value::Object(properties) = properties else {
        return Err(format!("{tool}: `properties` is not an object"));
    };
    if properties.contains_key(name) {
        return Err(format!(
            "{tool}: parameter `{name}` clashes with the argument added by the server"
        ));
    }
    properties.insert(name.to_owned(), property);
    if required {
        let list = schema
            .entry("required")
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(list) = list else {
            return Err(format!("{tool}: `required` is not an array"));
        };
        list.push(Value::String(name.to_owned()));
    }
    Ok(())
}

pub(crate) fn access_verb(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "reads",
        AccessLevel::Write => "creates or changes",
        AccessLevel::Destructive => "deletes or irreversibly changes",
    }
}

/// Название уровня доступа для сообщений и инструкций.
pub fn access_name(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read-only",
        AccessLevel::Write => "write",
        AccessLevel::Destructive => "destructive",
    }
}

/// Что пользователь делает, чтобы включить уровень `access`: права — настройка профиля
/// (флаги регистрации их не расширяют), на macOS повышение ещё и одобряется Touch ID.
/// Фраза продолжает «Ask the user to …».
pub fn access_hint(access: AccessLevel) -> String {
    match access {
        AccessLevel::Read => "keep the read-only access".to_owned(),
        level => format!(
            "allow `{}` access for the server's profile with `appstore-connect-mcp ui` and \
             restart the MCP client",
            access_name(level)
        ),
    }
}

#[cfg(test)]
mod tests {
    use appstore_connect_api::spec::toolsets;

    use super::*;
    use crate::composite::{Context, ToolFuture};
    use crate::toolsets::DEFAULT_TOOLSETS;

    fn options(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ServerOptions {
        ServerOptions {
            max_access,
            toolsets: toolsets.map(|t| t.iter().map(|s| s.to_string()).collect()),
            composite_tools: Vec::new(),
            ..ServerOptions::default()
        }
    }

    fn all() -> Option<&'static [&'static str]> {
        Some(&["all"])
    }

    /// Число инструментов: операции уровня и наборов плюс `api_request` и `api_search`.
    fn count(access: AccessLevel, toolsets: &[&str]) -> usize {
        operations()
            .iter()
            .filter(|op| access.allows(op.access) && toolsets.contains(&op.toolset))
            .count()
            + BUILT_IN
    }

    const BUILT_IN: usize = 2;

    fn all_slugs() -> Vec<&'static str> {
        toolsets().iter().map(|t| t.slug).collect()
    }

    fn run_nothing(_ctx: &Context, _args: Value) -> ToolFuture<'_> {
        Box::pin(async { Ok(json!({})) })
    }

    fn composite(name: &'static str, access: AccessLevel, files: bool) -> CompositeTool {
        CompositeTool {
            name,
            toolset: "builds",
            access,
            description: "Test tool.",
            input_schema: || json!({"type": "object", "additionalProperties": false, "properties": {}}),
            uses_local_files: files,
            run: run_nothing,
        }
    }

    #[test]
    fn schemas_match_operations() {
        assert!(!operations().is_empty());
        for op in operations() {
            let tool = operation_tool(op).unwrap();
            assert_eq!(tool.name, op.tool_name);
            assert!(op.tool_name.len() <= MAX_TOOL_NAME_LEN, "{}", op.tool_name);
            let expected: Value = serde_json::from_str(op.input_schema).unwrap();
            let mut actual = Value::Object((*tool.input_schema).clone());
            let props = actual["properties"].as_object_mut().unwrap();
            // Фильтр `jq` есть у всех операций с JSON-ответом и только у них.
            assert_eq!(
                props.remove(JQ_ARG).is_some(),
                !takes_save_to(op),
                "{}",
                op.tool_name
            );
            assert_eq!(
                props.remove(FLATTEN_ARG).is_some(),
                takes_flatten(op),
                "{}",
                op.tool_name
            );
            if takes_save_to(op) {
                assert!(props.remove(SAVE_TO_ARG).is_some(), "{}", op.tool_name);
                let required = actual["required"].as_array_mut().unwrap();
                assert_eq!(required.pop(), Some(json!(SAVE_TO_ARG)), "{}", op.tool_name);
                if required.is_empty() && expected.get("required").is_none() {
                    actual.as_object_mut().unwrap().remove("required");
                }
            }
            assert_eq!(actual, expected, "{}", op.tool_name);
            assert_eq!(actual["type"], "object", "{}", op.tool_name);
            assert_eq!(actual["additionalProperties"], false, "{}", op.tool_name);
            let description = tool.description.as_deref().unwrap();
            assert!(description.contains("Access: "), "{}", op.tool_name);
        }
    }

    #[test]
    fn annotations_follow_access_level() {
        for op in operations() {
            let tool = operation_tool(op).unwrap();
            let a = tool.annotations.as_ref().unwrap();
            assert_eq!(a.open_world_hint, Some(true));
            match op.access {
                AccessLevel::Read => {
                    assert_eq!(a.read_only_hint, Some(true), "{}", op.tool_name);
                    assert_eq!(a.destructive_hint, None, "{}", op.tool_name);
                }
                AccessLevel::Write | AccessLevel::Destructive => {
                    assert_eq!(a.read_only_hint, Some(false), "{}", op.tool_name);
                    assert_eq!(
                        a.destructive_hint,
                        Some(op.access == AccessLevel::Destructive),
                        "{}",
                        op.tool_name
                    );
                }
            }
        }
    }

    #[test]
    fn access_filtering() {
        let slugs = all_slugs();
        let read = ToolRegistry::new(&options(AccessLevel::Read, all())).unwrap();
        assert_eq!(read.len(), count(AccessLevel::Read, &slugs));
        assert!(read.iter().all(|t| t.access == AccessLevel::Read));
        assert!(
            read.iter().all(|t| {
                t.tool.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true)
            })
        );
        assert!(read.get("get_app").is_some());
        assert!(read.get("list_apps").is_some());

        let write = ToolRegistry::new(&options(AccessLevel::Write, all())).unwrap();
        assert_eq!(write.len(), count(AccessLevel::Write, &slugs));
        let destructive = ToolRegistry::new(&options(AccessLevel::Destructive, all())).unwrap();
        assert_eq!(destructive.len(), operations().len() + BUILT_IN);
        assert!(read.len() < write.len() && write.len() < destructive.len());
    }

    #[test]
    fn toolset_filtering() {
        let default = ToolRegistry::new(&options(AccessLevel::Destructive, None)).unwrap();
        assert_eq!(default.enabled_toolsets(), DEFAULT_TOOLSETS);
        assert_eq!(
            default.len(),
            count(AccessLevel::Destructive, DEFAULT_TOOLSETS)
        );
        let empty = ToolRegistry::new(&options(AccessLevel::Destructive, Some(&[]))).unwrap();
        assert_eq!(empty.len(), default.len());

        let chosen =
            ToolRegistry::new(&options(AccessLevel::Read, Some(&["analytics-reports"]))).unwrap();
        assert!(!chosen.is_empty());
        assert!(chosen.iter().all(|t| match t.kind {
            ToolKind::Operation(op) => op.toolset == "analytics-reports",
            ToolKind::ApiRequest | ToolKind::ApiSearch => true,
            ToolKind::Composite(_)
            | ToolKind::Custom(_)
            | ToolKind::ListToolsets
            | ToolKind::EnableToolset
            | ToolKind::DisableToolset => false,
        }));
        assert!(chosen.get("get_app").is_none());
        // Встроенные инструменты не зависят от наборов.
        assert!(chosen.get("api_request").is_some());
        assert!(chosen.get("api_search").is_some());

        let err =
            ToolRegistry::new(&options(AccessLevel::Read, Some(&["Приложения"]))).unwrap_err();
        assert!(err.contains("unknown toolset `Приложения`"), "{err}");
    }

    #[test]
    fn unavailable_reasons_name_the_flag() {
        let read = ToolRegistry::new(&options(AccessLevel::Read, all())).unwrap();
        let write_op = operations()
            .iter()
            .find(|op| op.access == AccessLevel::Write)
            .unwrap();
        let reason = read.unavailable_reason(write_op.tool_name).unwrap();
        assert!(reason.contains("`write` access"), "{reason}");
        let delete_op = operations()
            .iter()
            .find(|op| op.access == AccessLevel::Destructive)
            .unwrap();
        let reason = read.unavailable_reason(delete_op.tool_name).unwrap();
        assert!(reason.contains("`destructive` access"), "{reason}");
        assert!(read.unavailable_reason("get_app").is_none());
        assert!(read.unavailable_reason("no_such_tool").is_none());

        let analytics =
            ToolRegistry::new(&options(AccessLevel::Read, Some(&["analytics-reports"]))).unwrap();
        let reason = analytics.unavailable_reason("get_app").unwrap();
        assert!(reason.contains("toolset `apps`"), "{reason}");
        assert!(reason.contains("--toolsets"), "{reason}");
        assert!(
            reason.contains("api_request") && reason.contains("GET /v1/apps/{id}"),
            "{reason}"
        );
    }

    #[test]
    fn binary_operations_need_local_files() {
        let without = ToolRegistry::new(&ServerOptions {
            local_files: false,
            ..options(AccessLevel::Destructive, all())
        })
        .unwrap();
        let file_tools: Vec<&str> = operations()
            .iter()
            .filter(|op| uses_local_files(op))
            .map(|op| op.tool_name)
            .collect();
        assert!(!file_tools.is_empty());
        for name in &file_tools {
            assert!(without.get(name).is_none(), "{name}");
            let reason = without.unavailable_reason(name).unwrap();
            assert!(reason.contains("local files are disabled"), "{reason}");
        }
        assert_eq!(
            without.len(),
            operations().len() - file_tools.len() + BUILT_IN
        );
    }

    #[test]
    fn composite_tools_follow_access_toolsets_and_files() {
        let tools = vec![
            composite("test_read", AccessLevel::Read, false),
            composite("test_write", AccessLevel::Write, false),
            composite("test_files", AccessLevel::Read, true),
        ];
        let read = ToolRegistry::new(&ServerOptions {
            composite_tools: tools.clone(),
            ..options(AccessLevel::Read, None)
        })
        .unwrap();
        let tool = read.get("test_read").unwrap();
        assert!(matches!(tool.kind, ToolKind::Composite(0)));
        assert!(
            tool.tool
                .description
                .as_deref()
                .unwrap()
                .contains("Access: read-only.")
        );
        assert!(read.get("test_files").is_some());
        assert!(read.get("test_write").is_none());
        assert!(
            read.unavailable_reason("test_write")
                .unwrap()
                .contains("appstore-connect-mcp ui")
        );

        let no_files = ToolRegistry::new(&ServerOptions {
            composite_tools: tools.clone(),
            local_files: false,
            ..options(AccessLevel::Write, None)
        })
        .unwrap();
        assert!(no_files.get("test_write").is_some());
        assert!(no_files.get("test_files").is_none());

        let other = ToolRegistry::new(&ServerOptions {
            composite_tools: tools,
            ..options(AccessLevel::Read, Some(&["apps"]))
        })
        .unwrap();
        let reason = other.unavailable_reason("test_read").unwrap();
        assert!(
            reason.contains("toolset `builds`") && !reason.contains("api_request"),
            "{reason}"
        );

        for bad in [
            vec![composite("get_app", AccessLevel::Read, false)],
            vec![composite("api_request", AccessLevel::Read, false)],
            vec![
                composite("twice", AccessLevel::Read, false),
                composite("twice", AccessLevel::Read, false),
            ],
            vec![CompositeTool {
                toolset: "nope",
                ..composite("x", AccessLevel::Read, false)
            }],
        ] {
            assert!(
                ToolRegistry::new(&ServerOptions {
                    composite_tools: bad,
                    ..options(AccessLevel::Read, None)
                })
                .is_err()
            );
        }
    }
}
