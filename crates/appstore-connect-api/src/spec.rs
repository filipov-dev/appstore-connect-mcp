//! Описание операций App Store Connect API.
//!
//! Таблицы [`operations`], [`excluded_operations`] и [`toolsets`] строит `build.rs`
//! из снимка спецификации `spec/openapi.oas.json` и файла-дополнения `spec/overlay.toml`.
//! Типы этого модуля — контракт между генератором, исполнителем запросов и MCP-сервером.
//!
//! Как MCP-сервер строит запрос из аргументов инструмента:
//!
//! 1. Проверить аргументы по [`OperationDef::input_schema`] (неизвестные, недостающие
//!    и аргументы неверного типа отклоняются).
//! 2. Подставить [`ParamLocation::Path`] в шаблон [`OperationDef::path`] (значения
//!    кодируются как сегмент пути), [`ParamLocation::Query`] передать в строке запроса
//!    под именем [`ParamDef::api_name`] (аргумент `filter.app` → `filter[app]`; массивы
//!    JSON:API — `fields[apps]`, `include`, `filter[…]` — через запятую).
//! 3. Тело — аргумент `body`, отправляется JSON-телом ([`BodyKind::Json`]).
//! 4. Отправить заголовок `Accept` из [`OperationDef::accept`] и разобрать ответ по
//!    [`OperationDef::response`]: JSON или бинарное содержимое (например, gzip-отчёты продаж).
//!
//! Заголовок `Authorization` (JWT) в параметры не входит: его добавляет исполнитель.

/// HTTP-метод операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// Уровень доступа операции (из файла-дополнения или по HTTP-методу).
/// Уровни упорядочены: `Read < Write < Destructive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccessLevel {
    Read,
    Write,
    Destructive,
}

impl AccessLevel {
    /// Разрешает ли текущий уровень операцию с уровнем `required`.
    pub fn allows(self, required: AccessLevel) -> bool {
        required <= self
    }

    /// Имя уровня: `read`, `write` или `destructive`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Destructive => "destructive",
        }
    }
}

/// Где передаётся параметр операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamLocation {
    /// Подставляется в шаблон пути (`{name}`).
    Path,
    /// Передаётся в строке запроса.
    Query,
    /// JSON-тело запроса; у операции не больше одного такого параметра, его имя — `body`.
    Body,
}

/// Тип значения параметра в аргументах инструмента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    String,
    Integer,
    Number,
    Boolean,
    /// Массив; в строке запроса JSON:API передаётся через запятую (`include=a,b`).
    Array,
    /// JSON-объект (тело запроса).
    Object,
}

/// Вид тела запроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// `application/json`: аргумент `body` ([`ParamLocation::Body`]).
    Json,
}

/// Вид успешного ответа.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    /// JSON (или пустой ответ, например `204`).
    Json,
    /// Содержимое файла (`application/octet-stream`, `application/a-gzip` и т. п.).
    Binary,
}

/// Параметр операции.
#[derive(Debug, Clone, Copy)]
pub struct ParamDef {
    /// Ключ в аргументах инструмента. У параметров JSON:API скобки заменены точкой
    /// (`filter[app]` → `filter.app`): API Claude принимает в именах свойств схемы только
    /// `[a-zA-Z0-9_.-]`.
    pub name: &'static str,
    /// Имя в пути или строке запроса, как в спецификации: `id`, `filter[app]`, `fields[apps]`.
    /// У тела — `body`.
    pub api_name: &'static str,
    pub location: ParamLocation,
    /// Обязателен ли параметр. Параметры пути обязательны всегда.
    pub required: bool,
    pub description: &'static str,
    pub value_type: ParamType,
}

/// Набор инструментов: несколько тегов спецификации под одним slug.
#[derive(Debug, Clone, Copy)]
pub struct ToolsetDef {
    /// Короткое английское имя для `--toolsets`, например `testflight`.
    pub slug: &'static str,
    /// Описание набора.
    pub description: &'static str,
    /// Теги спецификации, входящие в набор.
    pub tags: &'static [&'static str],
}

/// Операция App Store Connect API, доступная как MCP-инструмент.
#[derive(Debug, Clone, Copy)]
pub struct OperationDef {
    /// Имя MCP-инструмента («глагол_ресурс», не длиннее 40 символов), уникально.
    pub tool_name: &'static str,
    /// Исходный `operationId` из спецификации.
    pub operation_id: &'static str,
    /// Slug набора инструментов ([`ToolsetDef::slug`]).
    pub toolset: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    pub method: HttpMethod,
    /// Шаблон пути с версией API, например `/v1/apps/{id}`.
    pub path: &'static str,
    pub access: AccessLevel,
    pub params: &'static [ParamDef],
    /// Вид тела запроса; `None` — тела нет.
    pub body: Option<BodyKind>,
    pub response: ResponseKind,
    /// Значение заголовка `Accept`: `application/json`, тип файла (`application/a-gzip`,
    /// `text/csv`) или JSON-тип Apple (`application/vnd.apple.xcode-metrics+json`).
    pub accept: &'static str,
    /// JSON Schema аргументов инструмента в виде JSON-текста.
    ///
    /// Объект с `type: object` и `additionalProperties: false`: параметры пути и запроса
    /// и `body` (JSON-тело с раскрытыми `$ref`). Заголовка авторизации в схеме нет.
    pub input_schema: &'static str,
}

impl OperationDef {
    /// Параметры в расположении `location`.
    pub fn params_in(&self, location: ParamLocation) -> impl Iterator<Item = &'static ParamDef> {
        self.params
            .iter()
            .filter(move |param| param.location == location)
    }

    /// Параметр по имени.
    pub fn param(&self, name: &str) -> Option<&'static ParamDef> {
        self.params.iter().find(|param| param.name == name)
    }
}

/// Почему операция спецификации не зарегистрирована как MCP-инструмент.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExclusionReason {
    /// Операция устарела (`deprecated: true` в спецификации).
    Deprecated,
    /// Чтение связи (`GET …/relationships/…`): только `type` и `id` связанных ресурсов.
    RelationshipLinkage,
    /// Список связанных ресурсов, повторяющий общий список с фильтром по родителю.
    DuplicateList,
}

impl ExclusionReason {
    /// Код причины: `deprecated`, `relationship_linkage`, `duplicate_list`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deprecated => "deprecated",
            Self::RelationshipLinkage => "relationship_linkage",
            Self::DuplicateList => "duplicate_list",
        }
    }

    /// Объяснение для агента (английский).
    pub fn explanation(self) -> &'static str {
        match self {
            Self::Deprecated => "The operation is deprecated in the App Store Connect API.",
            Self::RelationshipLinkage => {
                "The operation returns only the type and ID of related resources."
            }
            Self::DuplicateList => {
                "The operation duplicates a top-level list filtered by the parent resource."
            }
        }
    }
}

/// Операция спецификации, которая не зарегистрирована как MCP-инструмент. Её находит
/// поиск по операциям, а вызвать её можно через `api_request`.
#[derive(Debug, Clone, Copy)]
pub struct ExcludedOperationDef {
    pub operation_id: &'static str,
    /// Slug набора инструментов, к которому относится тег операции.
    pub toolset: &'static str,
    pub summary: &'static str,
    pub method: HttpMethod,
    /// Шаблон пути с версией API.
    pub path: &'static str,
    /// Уровень доступа, нужный для вызова через `api_request`.
    pub access: AccessLevel,
    pub reason: ExclusionReason,
    /// Что вызвать вместо операции (английский текст для агента); пустая строка — замены нет.
    pub alternative: &'static str,
}

// Таблицы `TOOLSETS`, `OPERATIONS` и `EXCLUDED` генерирует `build.rs`.
include!(concat!(env!("OUT_DIR"), "/operations.rs"));

/// Все операции API, отсортированные по имени инструмента.
pub fn operations() -> &'static [OperationDef] {
    OPERATIONS
}

/// Операция по имени MCP-инструмента.
pub fn find(tool_name: &str) -> Option<&'static OperationDef> {
    OPERATIONS
        .binary_search_by(|op| op.tool_name.cmp(tool_name))
        .ok()
        .and_then(|index| OPERATIONS.get(index))
}

/// Операции спецификации, не зарегистрированные как инструменты, отсортированные
/// по `operationId`.
pub fn excluded_operations() -> &'static [ExcludedOperationDef] {
    EXCLUDED
}

/// Исключённая операция по `operationId`.
pub fn find_excluded(operation_id: &str) -> Option<&'static ExcludedOperationDef> {
    EXCLUDED
        .binary_search_by(|op| op.operation_id.cmp(operation_id))
        .ok()
        .and_then(|index| EXCLUDED.get(index))
}

/// Наборы инструментов в порядке таблицы дополнения.
pub fn toolsets() -> &'static [ToolsetDef] {
    TOOLSETS
}

/// Набор инструментов по slug.
pub fn find_toolset(slug: &str) -> Option<&'static ToolsetDef> {
    TOOLSETS.iter().find(|toolset| toolset.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_levels_are_ordered() {
        assert!(AccessLevel::Destructive.allows(AccessLevel::Write));
        assert!(AccessLevel::Write.allows(AccessLevel::Read));
        assert!(!AccessLevel::Read.allows(AccessLevel::Write));
        assert!(!AccessLevel::Write.allows(AccessLevel::Destructive));
    }
}
