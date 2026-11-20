//! Инструмент `api_search`: поиск по всем операциям App Store Connect API без обращения к API.
//!
//! Ищет среди инструментов операций, исключённых операций (их вызывают через
//! `api_request`) и составных инструментов — в том числе незарегистрированных на этом
//! сервере. Каждый результат говорит, можно ли вызвать его здесь, а если нет — почему
//! (причина исключения и замена, набор не включён, уровень доступа выше) и что его включает.
//!
//! Слова запроса сравниваются с именем инструмента, `operationId`, кратким описанием,
//! сегментами пути (camelCase разбивается на слова) и набором. Русские слова переводятся
//! словарём терминов App Store Connect ([`RU_EN`]: «сборка» → `build`, «тестировщик» →
//! `tester`…), английские сравниваются по началу слова, поэтому `build` находит и `builds`,
//! и `buildBetaDetails`.
//!
//! С аргументом `tool_name` возвращается полное описание одного инструмента со схемой
//! аргументов.

use std::sync::Arc;

use appstore_connect_api::spec::{
    AccessLevel, ExcludedOperationDef, OperationDef, excluded_operations, find, operations,
};
use rmcp::model::{JsonObject, Tool};
use serde_json::{Value, json};

use crate::api_request::API_REQUEST_TOOL;
use crate::composite::CompositeTool;
use crate::error::ToolError;
use crate::registry::{
    ToolRegistry, access_hint, access_name, annotations, composite_tool, operation_tool,
};
use crate::request::kind;

/// Имя инструмента.
pub const API_SEARCH_TOOL: &str = "api_search";

/// Результатов по умолчанию и наибольшее число результатов.
const DEFAULT_LIMIT: usize = 15;
const MAX_LIMIT: usize = 50;

/// Русские основы слов и английские термины App Store Connect. Основа сравнивается с началом
/// слова запроса, поэтому падежи и числа («сборки», «сборок») находятся одной строкой.
pub const RU_EN: &[(&str, &[&str])] = &[
    ("список", &["list"]),
    ("перечен", &["list"]),
    ("получ", &["get"]),
    ("созда", &["create"]),
    ("нов", &["create"]),
    ("удал", &["delete"]),
    ("измен", &["update"]),
    ("обнов", &["update"]),
    ("добав", &["add", "create"]),
    ("приложени", &["app"]),
    ("апп", &["app"]),
    ("сборк", &["build"]),
    ("билд", &["build"]),
    ("верси", &["version"]),
    ("отзыв", &["review", "customer"]),
    ("ревью", &["review"]),
    ("модераци", &["review", "submission"]),
    ("проверк", &["review"]),
    ("ответ", &["response"]),
    ("тестировщик", &["tester", "beta"]),
    ("тестер", &["tester", "beta"]),
    ("бета", &["beta"]),
    ("групп", &["group"]),
    ("скриншот", &["screenshot"]),
    ("снимк", &["screenshot"]),
    ("превью", &["preview"]),
    ("видео", &["preview", "video"]),
    ("подписк", &["subscription"]),
    ("покупк", &["purchase"]),
    ("встроенн", &["inapppurchase"]),
    ("цен", &["price"]),
    ("стоимост", &["price"]),
    ("территори", &["territory"]),
    ("стран", &["territory"]),
    ("доступност", &["availability"]),
    ("сертификат", &["certificate"]),
    ("профил", &["profile"]),
    ("устройств", &["device"]),
    ("идентификатор", &["bundleid", "id"]),
    ("бандл", &["bundleid"]),
    ("пакет", &["bundleid", "package"]),
    ("возможност", &["capability"]),
    ("отчёт", &["report"]),
    ("отчет", &["report"]),
    ("продаж", &["sales"]),
    ("финанс", &["finance"]),
    ("аналитик", &["analytics"]),
    ("пользовател", &["user"]),
    ("приглашени", &["invitation"]),
    ("локализаци", &["localization"]),
    ("язык", &["localization"]),
    ("описани", &["localization", "description"]),
    ("ключев", &["keyword"]),
    ("категори", &["category"]),
    ("возраст", &["agerating"]),
    ("рейтинг", &["rating"]),
    ("релиз", &["release"]),
    ("выпуск", &["release"]),
    ("поэтапн", &["phasedrelease"]),
    ("отправк", &["submission"]),
    ("подач", &["submission"]),
    ("заявк", &["submission", "request"]),
    ("рабоч", &["workflow"]),
    ("ветк", &["branch"]),
    ("репозитори", &["repository"]),
    ("событи", &["event"]),
    ("страниц", &["page"]),
    ("игров", &["gamecenter"]),
    ("достижени", &["achievement"]),
    ("лидер", &["leaderboard"]),
    ("вебхук", &["webhook"]),
    ("уведомлени", &["webhook", "notification"]),
    ("песочниц", &["sandbox"]),
    ("предзаказ", &["preorder"]),
    ("промокод", &["offercode"]),
    ("предложени", &["offer"]),
    ("скидк", &["offer"]),
    ("шифровани", &["encryption"]),
    ("соглашени", &["agreement"]),
    ("лицензи", &["agreement", "eula"]),
    ("иконк", &["icon"]),
    ("значк", &["icon"]),
    ("клип", &["appclip"]),
    ("загрузк", &["upload"]),
    ("файл", &["file"]),
    ("ключ", &["key"]),
    ("тест", &["test"]),
    ("журнал", &["log"]),
    ("инструкци", &["instruction", "testflight"]),
    ("истори", &["history"]),
    ("состояни", &["state", "status"]),
    ("статус", &["state", "status"]),
];

/// Описание инструмента.
pub fn api_search_tool() -> Tool {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {
                "type": "string",
                "description": "Words to search for, in English or Russian, for example \
                    `beta tester invite`, `phased release`, `сборки testflight`, `/v1/apps`."
            },
            "tool_name": {
                "type": "string",
                "description": "Return the full description and argument schema of this tool \
                    (registered here or not) instead of searching."
            },
            "toolset": {
                "type": "string",
                "description": "Search only in this toolset."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "description": "Maximum number of results (default 15)."
            }
        }
    });
    let schema = match schema {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };
    let mut tool = Tool::new(
        API_SEARCH_TOOL,
        "Search all App Store Connect API operations by words in English or Russian (tool \
         names, summaries, paths, toolsets), including tools that are not enabled on this \
         server and operations without a tool. Each result says whether it can be called here \
         and, if not, why and which flag, toolset or `api_request` call to use. Pass \
         `tool_name` to get the full description and argument schema of one tool. Does not call \
         the API.",
        Arc::new(schema),
    );
    tool.title = Some("Search App Store Connect API operations".to_owned());
    tool.annotations = Some(annotations(
        "Search App Store Connect API operations",
        AccessLevel::Read,
        None,
    ));
    tool
}

/// Запись поиска.
enum Entry<'a> {
    Operation(&'static OperationDef),
    Excluded(&'static ExcludedOperationDef),
    Composite(&'a CompositeTool),
}

impl Entry<'_> {
    /// Длина имени и пути: при равном весе первыми идут более общие записи (`list_apps`
    /// раньше `list_app_accessibility_declarations`).
    fn size(&self) -> usize {
        match self {
            Self::Operation(op) => op.tool_name.len() + op.path.len(),
            Self::Excluded(op) => op.operation_id.len() + op.path.len(),
            Self::Composite(tool) => tool.name.len(),
        }
    }

    fn toolset(&self) -> &str {
        match self {
            Self::Operation(op) => op.toolset,
            Self::Excluded(op) => op.toolset,
            Self::Composite(tool) => tool.toolset,
        }
    }

    /// Поля для поиска с весами: имя важнее пути, путь важнее описания.
    fn fields(&self) -> Vec<(String, u32)> {
        match self {
            Self::Operation(op) => vec![
                (op.tool_name.replace('_', " "), 4),
                (split_camel(op.operation_id), 2),
                (split_camel(op.path), 3),
                (op.summary.to_owned(), 2),
                (op.toolset.replace('-', " "), 1),
            ],
            Self::Excluded(op) => vec![
                (split_camel(op.operation_id), 3),
                (split_camel(op.path), 3),
                (op.summary.to_owned(), 2),
                (op.toolset.replace('-', " "), 1),
            ],
            Self::Composite(tool) => vec![
                (tool.name.replace('_', " "), 4),
                (tool.description.to_owned(), 1),
                (tool.toolset.replace('-', " "), 1),
            ],
        }
    }
}

/// Выполняет `api_search`.
pub fn search(args: &JsonObject, registry: &ToolRegistry) -> Result<String, ToolError> {
    for key in args.keys() {
        if !["query", "tool_name", "toolset", "limit"].contains(&key.as_str()) {
            return Err(ToolError::new(format!(
                "unknown argument `{key}`; allowed arguments: `query`, `tool_name`, `toolset`, \
                 `limit`"
            )));
        }
    }
    let text_arg = |name: &str| -> Result<Option<String>, ToolError> {
        match args.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim().to_owned())),
            Some(other) => Err(ToolError::new(format!(
                "argument `{name}` must be a string, got {}",
                kind(other)
            ))),
        }
    };
    if let Some(name) = text_arg("tool_name")? {
        return describe_tool(&name, registry);
    }
    let Some(query) = text_arg("query")? else {
        return Err(ToolError::new(
            "pass `query` (words to search for) or `tool_name` (a tool to describe)",
        ));
    };
    let toolset = text_arg("toolset")?;
    let limit = match args.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=MAX_LIMIT as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| {
                ToolError::new(format!(
                    "argument `limit` must be an integer from 1 to {MAX_LIMIT}"
                ))
            })?,
    };

    let terms = terms(&query);
    if terms.is_empty() {
        return Err(ToolError::new(
            "the query has no searchable words; use words such as `build`, `beta tester`, \
             `screenshot`",
        ));
    }
    let mut entries: Vec<Entry<'_>> = operations().iter().map(Entry::Operation).collect();
    entries.extend(excluded_operations().iter().map(Entry::Excluded));
    entries.extend(registry.composite_tools().iter().map(Entry::Composite));

    let mut scored: Vec<(usize, u32, Entry<'_>)> = entries
        .into_iter()
        .filter(|entry| toolset.as_deref().is_none_or(|t| entry.toolset() == t))
        .filter_map(|entry| {
            let (matched, score) = score(&entry, &terms);
            (matched > 0).then_some((matched, score, entry))
        })
        .collect();
    // Сначала записи, где нашлись все слова запроса, затем по весу.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(a.2.size().cmp(&b.2.size()))
    });
    let best = scored.first().map_or(0, |s| s.0);
    // Если все слова нашлись хоть где-то — только такие записи. Иначе — лучшие по
    // числу слов и весу: запрос из нескольких тем («release status metadata export»)
    // должен показать инструменты каждой темы, а не одну верхнюю группу.
    let complete = best == terms.len();
    let total = scored.iter().filter(|s| !complete || s.0 == best).count();
    let results: Vec<Value> = scored
        .iter()
        .filter(|s| !complete || s.0 == best)
        .take(limit)
        .map(|(_, _, entry)| describe_entry(entry, registry))
        .collect();
    let mut out = json!({
        "query": query,
        "matches": total,
        "results": results,
    });
    if !complete && let Value::Object(map) = &mut out {
        map.insert(
            "note".to_owned(),
            json!("no result contains every word of the query; these match some of them"),
        );
    }
    if total > limit
        && let Value::Object(map) = &mut out
    {
        map.insert(
            "more".to_owned(),
            json!(format!(
                "{} more results; add words, pass `toolset` or raise `limit`",
                total - limit
            )),
        );
    }
    Ok(out.to_string())
}

/// Поисковые термины: русские слова — по словарю, английские — как есть, в нижнем регистре.
/// Каждый элемент — варианты одного слова запроса (достаточно совпадения любого).
fn terms(query: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for word in split_camel(query)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_lowercase();
        if word.chars().any(|c| matches!(c, 'а'..='я' | 'ё')) {
            let translated: Vec<String> = RU_EN
                .iter()
                .filter(|(stem, _)| word.starts_with(stem))
                .flat_map(|(_, en)| en.iter().map(|t| (*t).to_owned()))
                .collect();
            if !translated.is_empty() {
                out.push(translated);
            }
            continue;
        }
        if STOP_WORDS.contains(&word.as_str()) || (word.len() < 2 && !word.starts_with('v')) {
            continue;
        }
        out.push(vec![singular(&word)]);
    }
    out
}

const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "of", "for", "to", "in", "on", "and", "or", "with", "by", "my", "all",
];

/// Единственное число английского слова (грубо): `builds` → `build`, `categories` → `categor`.
fn singular(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies").filter(|s| s.len() > 2) {
        return stem.to_owned();
    }
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
        return word[..word.len() - 1].to_owned();
    }
    word.to_owned()
}

/// Разбивает camelCase и разделители на слова: `betaTesterInvitations` → `beta tester
/// invitations`, `/v1/apps/{id}` → ` v1 apps  id `.
fn split_camel(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut prev_lower = false;
    for c in text.chars() {
        if c.is_uppercase() && prev_lower {
            out.push(' ');
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else {
            out.push(' ');
        }
    }
    out
}

/// Сколько слов запроса нашлось и суммарный вес.
fn score(entry: &Entry<'_>, terms: &[Vec<String>]) -> (usize, u32) {
    let fields: Vec<(Vec<String>, String, u32)> = entry
        .fields()
        .into_iter()
        .map(|(text, weight)| {
            let lower = split_camel(&text);
            let words = lower.split_whitespace().map(str::to_owned).collect();
            // Слитная запись поля: `inapppurchase` находит `inAppPurchases`.
            let joined = lower.split_whitespace().collect::<String>();
            (words, joined, weight)
        })
        .collect();
    let mut matched = 0;
    let mut total = 0;
    for variants in terms {
        // Целое слово весит вдвое больше начала слова.
        let best = fields
            .iter()
            .filter_map(|(words, joined, weight)| {
                let exact = variants
                    .iter()
                    .any(|term| words.iter().any(|w| w == term || singular(w) == *term));
                let partial = variants.iter().any(|term| {
                    words.iter().any(|w| w.starts_with(term.as_str()))
                        || (term.len() > 6 && joined.contains(term.as_str()))
                });
                if exact {
                    Some(weight * 2)
                } else if partial {
                    Some(*weight)
                } else {
                    None
                }
            })
            .max();
        if let Some(weight) = best {
            matched += 1;
            total += weight;
        }
    }
    (matched, total)
}

fn describe_entry(entry: &Entry<'_>, registry: &ToolRegistry) -> Value {
    match entry {
        Entry::Operation(op) => {
            let mut value = json!({
                "tool": op.tool_name,
                "summary": op.summary,
                "method": op.method.as_str(),
                "path": op.path,
                "operation_id": op.operation_id,
                "toolset": op.toolset,
                "access": op.access.as_str(),
            });
            let status = match registry.unavailable_reason(op.tool_name) {
                None => "available".to_owned(),
                Some(reason) => format!(
                    "not registered: {}",
                    with_api_request(&reason, op.access, registry)
                ),
            };
            insert(&mut value, "status", json!(status));
            value
        }
        Entry::Excluded(op) => {
            let mut status = format!(
                "no tool: {} Call it with `{API_REQUEST_TOOL}` ({} {})",
                op.reason.explanation(),
                op.method.as_str(),
                op.path
            );
            if !op.alternative.is_empty() {
                status.push_str(". Better: ");
                status.push_str(op.alternative);
            }
            if !registry.max_access().allows(op.access) {
                status.push_str(&format!(
                    ". It needs {} access: ask the user to {}",
                    access_name(op.access),
                    access_hint(op.access)
                ));
            }
            json!({
                "operation_id": op.operation_id,
                "summary": op.summary,
                "method": op.method.as_str(),
                "path": op.path,
                "toolset": op.toolset,
                "access": op.access.as_str(),
                "excluded": op.reason.as_str(),
                "status": status,
            })
        }
        Entry::Composite(tool) => {
            let status = match registry.unavailable_reason(tool.name) {
                None if registry.get(tool.name).is_some() => "available".to_owned(),
                None => "not registered".to_owned(),
                Some(reason) => format!("not registered: {reason}"),
            };
            json!({
                "tool": tool.name,
                "summary": first_sentence(tool.description),
                "kind": "composite",
                "toolset": tool.toolset,
                "access": tool.access.as_str(),
                "status": status,
            })
        }
    }
}

/// К причине «набор не включён» `unavailable_reason` уже добавляет `api_request`; к причине
/// «уровень доступа» — нет: `api_request` тоже не превышает уровень сервера.
fn with_api_request(reason: &str, access: AccessLevel, registry: &ToolRegistry) -> String {
    if registry.max_access().allows(access) || reason.contains(API_REQUEST_TOOL) {
        reason.to_owned()
    } else {
        format!("{reason}; `{API_REQUEST_TOOL}` is limited to the same access level")
    }
}

fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    match text.find(". ") {
        Some(end) => &text[..=end],
        None => text,
    }
}

fn insert(value: &mut Value, key: &str, item: Value) {
    if let Value::Object(map) = value {
        map.insert(key.to_owned(), item);
    }
}

/// Полное описание одного инструмента (`tool_name`).
fn describe_tool(name: &str, registry: &ToolRegistry) -> Result<String, ToolError> {
    let (tool, status) = if let Some(registered) = registry.get(name) {
        (registered.tool.clone(), "available".to_owned())
    } else if let Some(op) = find(name) {
        let tool = operation_tool(op).map_err(ToolError::new)?;
        let reason = registry
            .unavailable_reason(name)
            .unwrap_or_else(|| "not registered".to_owned());
        (tool, format!("not registered: {reason}"))
    } else if let Some(composite) = registry.composite_tools().iter().find(|t| t.name == name) {
        let tool = composite_tool(composite).map_err(ToolError::new)?;
        let reason = registry
            .unavailable_reason(name)
            .unwrap_or_else(|| "not registered".to_owned());
        (tool, format!("not registered: {reason}"))
    } else {
        return Err(ToolError::new(format!(
            "no tool named `{name}`; search with `query` to find the right name"
        )));
    };
    let mut value = json!({
        "tool": tool.name,
        "status": status,
        "description": tool.description,
        "input_schema": Value::Object((*tool.input_schema).clone()),
    });
    if let Some(op) = find(name) {
        insert(&mut value, "method", json!(op.method.as_str()));
        insert(&mut value, "path", json!(op.path));
        insert(&mut value, "toolset", json!(op.toolset));
        insert(&mut value, "access", json!(op.access.as_str()));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerOptions;

    fn registry(access: AccessLevel, toolsets: Option<&[&str]>) -> ToolRegistry {
        ToolRegistry::new(&ServerOptions {
            max_access: access,
            toolsets: toolsets.map(|t| t.iter().map(|s| (*s).to_owned()).collect()),
            ..ServerOptions::default()
        })
        .unwrap()
    }

    fn run(registry: &ToolRegistry, args: Value) -> Value {
        let Value::Object(args) = args else {
            panic!("не объект");
        };
        serde_json::from_str(&search(&args, registry).unwrap()).unwrap()
    }

    fn tools(result: &Value) -> Vec<String> {
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["tool"].as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn finds_operations_by_english_words() {
        let registry = registry(AccessLevel::Read, None);
        let result = run(&registry, json!({"query": "list apps"}));
        assert!(tools(&result).contains(&"list_apps".to_owned()), "{result}");
        let result = run(&registry, json!({"query": "beta testers"}));
        assert!(
            tools(&result).iter().any(|t| t.contains("beta_tester")),
            "{result}"
        );
        let result = run(&registry, json!({"query": "/v1/apps/{id}"}));
        assert!(tools(&result).contains(&"get_app".to_owned()), "{result}");
    }

    #[test]
    fn partial_matches_cover_every_topic_of_the_query() {
        let registry = registry(AccessLevel::Read, None);
        let result = run(
            &registry,
            json!({"query": "release status metadata export report download profile download composite"}),
        );
        let found = tools(&result);
        for expected in [
            "release_status",
            "metadata_export",
            "report_download",
            "profile_download",
        ] {
            assert!(found.contains(&expected.to_owned()), "{expected}: {result}");
        }
        assert!(result["note"].is_string(), "{result}");
    }

    #[test]
    fn finds_operations_by_russian_words() {
        let registry = registry(AccessLevel::Read, None);
        for (query, expected) in [
            ("список приложений", "list_apps"),
            ("сборки", "build"),
            ("тестировщики", "tester"),
            ("скриншоты", "screenshot"),
            ("подписки", "subscription"),
            ("сертификаты", "certificate"),
            ("отчёт продаж", "sales_report"),
            ("отзывы", "review"),
            ("цены", "price"),
        ] {
            let result = run(&registry, json!({"query": query, "limit": 50}));
            assert!(
                tools(&result).iter().any(|t| t.contains(expected)),
                "{query}: {result}"
            );
        }
    }

    #[test]
    fn explains_unavailable_results() {
        let registry = registry(AccessLevel::Read, Some(&["apps"]));
        let result = run(&registry, json!({"query": "create beta group"}));
        let entry = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["tool"] == "create_beta_group")
            .unwrap();
        let status = entry["status"].as_str().unwrap();
        assert!(status.contains("`write` access"), "{status}");

        let result = run(&registry, json!({"query": "list sales reports"}));
        let entry = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["tool"] == "list_sales_reports")
            .unwrap();
        let status = entry["status"].as_str().unwrap();
        assert!(
            status.contains("toolset `analytics-reports`") && status.contains("--toolsets"),
            "{status}"
        );

        // Исключённые операции находятся с причиной и заменой.
        let result = run(
            &registry,
            json!({"query": "subcategories relationships", "limit": 50}),
        );
        let excluded = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["excluded"] == "relationship_linkage")
            .unwrap();
        let status = excluded["status"].as_str().unwrap();
        assert!(
            status.contains("api_request") && status.contains("Better:"),
            "{status}"
        );

        let result = run(&registry, json!({"query": "list apps"}));
        let entry = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["tool"] == "list_apps")
            .unwrap();
        assert_eq!(entry["status"], "available");
    }

    #[test]
    fn describes_one_tool_and_filters_by_toolset() {
        let registry = registry(AccessLevel::Read, None);
        let result = run(&registry, json!({"tool_name": "create_beta_group"}));
        assert!(
            result["status"]
                .as_str()
                .unwrap()
                .contains("`write` access")
        );
        assert!(result["input_schema"]["properties"].get("body").is_some());
        assert_eq!(result["method"], "POST");

        let result = run(
            &registry,
            json!({"query": "app", "toolset": "testflight", "limit": 50}),
        );
        for entry in result["results"].as_array().unwrap() {
            assert_eq!(entry["toolset"], "testflight", "{entry}");
        }

        let Value::Object(bad) = json!({"query": "x", "nope": 1}) else {
            unreachable!()
        };
        assert!(search(&bad, &registry).is_err());
        let Value::Object(none) = json!({}) else {
            unreachable!()
        };
        assert!(search(&none, &registry).is_err());
        let Value::Object(missing) = json!({"tool_name": "no_such_tool"}) else {
            unreachable!()
        };
        assert!(search(&missing, &registry).is_err());
    }

    #[test]
    fn splits_words() {
        assert_eq!(
            split_camel("betaTesterInvitations")
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["beta", "tester", "invitations"]
        );
        assert_eq!(singular("builds"), "build");
        assert_eq!(singular("categories"), "categor");
        assert_eq!(singular("access"), "access");
    }
}
