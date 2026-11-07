//! Файл-дополнение `spec/overlay.toml`: всё, чего нет в спецификации Apple.
//!
//! Операции в дополнении называются по `operationId` спецификации: он стабилен между
//! версиями спецификации, а имя инструмента выводит генератор. Ссылка на операцию,
//! которой нет в спецификации, — ошибка сборки, чтобы дополнение не устаревало молча.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overlay {
    /// Наборы инструментов в порядке вывода.
    pub toolsets: Vec<Toolset>,
    /// Исключения из уровня доступа по HTTP-методу.
    #[serde(default)]
    pub access: Vec<AccessRule>,
    pub naming: Naming,
    #[serde(default)]
    pub exclude: Exclude,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Toolset {
    pub slug: String,
    pub description: String,
    /// Теги спецификации (первый тег операции).
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Read,
    Write,
    Destructive,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessRule {
    pub level: Level,
    /// Короткое английское объяснение для агента: попадает в описание инструмента.
    pub reason: String,
    /// `operationId` операций.
    pub operations: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Naming {
    /// Замены в имени инструмента по порядку, пока имя длиннее предела: `[from, to]`.
    #[serde(default)]
    pub abbreviations: Vec<(String, String)>,
    /// Явные имена: `operationId` → имя инструмента.
    #[serde(default)]
    pub overrides: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exclude {
    /// Устаревшие операции, которые всё же регистрируются.
    #[serde(default)]
    pub keep_deprecated: Vec<String>,
    /// Чтения связей (`GET …/relationships/…`), которые всё же регистрируются.
    #[serde(default)]
    pub keep_relationship_reads: Vec<String>,
    /// Списки связанных ресурсов, повторяющие общий список с фильтром.
    #[serde(default)]
    pub duplicates: Vec<Duplicate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Duplicate {
    /// `operationId` исключаемого списка (`…_getToManyRelated`).
    pub operation: String,
    /// `operationId` общего списка, который возвращает то же самое.
    pub replacement: String,
    /// Параметр общего списка, в который передаётся идентификатор родителя (`filter[app]`).
    pub filter: String,
}

pub fn parse(raw: &str) -> Overlay {
    toml::from_str(raw).unwrap_or_else(|err| panic!("spec/overlay.toml: {err}"))
}
