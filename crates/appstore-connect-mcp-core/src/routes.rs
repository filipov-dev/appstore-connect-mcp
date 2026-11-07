//! Сопоставление конкретного пути запроса с шаблонами операций спецификации.
//!
//! Нужен `api_request`: уровень доступа произвольного запроса определяется по операции,
//! которой он соответствует (в том числе по исключённым операциям — их вызывают именно
//! через `api_request`). Сопоставление намеренно широкое, чтобы запрос нельзя было провести
//! по более низкому уровню, записав путь иначе:
//!
//! - сегменты пути сравниваются после percent-декодирования (`%61pps` — это `apps`);
//! - литералы сравниваются без учёта регистра;
//! - параметр шаблона (`{id}`) совпадает с любым непустым сегментом;
//! - завершающий `/` не учитывается;
//! - версия API сравнивается точно: `/v1/…` и `/v2/…` — разные операции.
//!
//! Если запросу соответствует несколько операций, берётся наибольший уровень доступа.
//! Если ни одной — уровень по методу: `GET` — чтение, `DELETE` — удаление, остальные —
//! запись.

use appstore_connect_api::spec::{
    AccessLevel, ExcludedOperationDef, HttpMethod, OperationDef, excluded_operations, operations,
};
use percent_encoding::percent_decode_str;

/// Операция спецификации, которой соответствует запрос.
#[derive(Debug, Clone, Copy)]
pub enum RouteMatch {
    Operation(&'static OperationDef),
    Excluded(&'static ExcludedOperationDef),
}

impl RouteMatch {
    pub fn access(&self) -> AccessLevel {
        match self {
            Self::Operation(op) => op.access,
            Self::Excluded(op) => op.access,
        }
    }

    pub fn path(&self) -> &'static str {
        match self {
            Self::Operation(op) => op.path,
            Self::Excluded(op) => op.path,
        }
    }

    /// Число параметров в шаблоне: чем меньше, тем точнее совпадение.
    fn params(&self) -> usize {
        self.path().matches('{').count()
    }
}

/// Уровень доступа запроса и самая точная операция, которой он соответствует.
#[derive(Debug, Clone, Copy)]
pub struct Route {
    pub access: AccessLevel,
    /// Самое точное совпадение (меньше всего параметров); `None` — путь неизвестен.
    pub matched: Option<RouteMatch>,
}

/// Все операции (и исключённые), которым соответствует запрос `method path`. `path` —
/// относительный путь без query.
pub fn match_route(method: HttpMethod, path: &str) -> Vec<RouteMatch> {
    let segments = split(path);
    let mut found = Vec::new();
    for op in operations() {
        if op.method == method && matches(op.path, &segments) {
            found.push(RouteMatch::Operation(op));
        }
    }
    for op in excluded_operations() {
        if op.method == method && matches(op.path, &segments) {
            found.push(RouteMatch::Excluded(op));
        }
    }
    found.sort_by_key(RouteMatch::params);
    found
}

/// Уровень доступа запроса (см. описание модуля).
pub fn route(method: HttpMethod, path: &str) -> Route {
    let found = match_route(method, path);
    let access = found
        .iter()
        .map(RouteMatch::access)
        .max()
        .unwrap_or_else(|| access_by_method(method));
    Route {
        access,
        matched: found.first().copied(),
    }
}

/// Уровень доступа по методу, когда путь не совпал ни с одной операцией.
pub fn access_by_method(method: HttpMethod) -> AccessLevel {
    match method {
        HttpMethod::Get => AccessLevel::Read,
        HttpMethod::Delete => AccessLevel::Destructive,
        HttpMethod::Post | HttpMethod::Put | HttpMethod::Patch => AccessLevel::Write,
    }
}

/// Сегменты пути после percent-декодирования, в нижнем регистре, без пустого последнего.
fn split(path: &str) -> Vec<String> {
    let trimmed = path.trim_start_matches('/');
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    trimmed
        .split('/')
        .map(|segment| {
            percent_decode_str(segment)
                .decode_utf8_lossy()
                .to_lowercase()
        })
        .collect()
}

fn matches(template: &str, segments: &[String]) -> bool {
    let parts: Vec<&str> = template.trim_start_matches('/').split('/').collect();
    parts.len() == segments.len()
        && parts.iter().zip(segments).all(|(part, segment)| {
            if part.starts_with('{') && part.ends_with('}') {
                !segment.is_empty()
            } else {
                part.eq_ignore_ascii_case(segment)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_operations_and_excluded() {
        let found = match_route(HttpMethod::Get, "/v1/apps/123");
        assert!(
            found
                .iter()
                .any(|m| matches!(m, RouteMatch::Operation(op) if op.tool_name == "get_app"))
        );
        let route = route(HttpMethod::Get, "/v1/apps/123");
        assert_eq!(route.access, AccessLevel::Read);
        assert!(
            matches!(route.matched, Some(RouteMatch::Operation(op)) if op.tool_name == "get_app")
        );

        // Исключённая операция (чтение связи) тоже находится.
        let route = super::route(
            HttpMethod::Get,
            "/v1/appCategories/GAMES/relationships/subcategories",
        );
        assert!(
            matches!(route.matched, Some(RouteMatch::Excluded(_))),
            "{route:?}"
        );
    }

    #[test]
    fn spelling_does_not_lower_the_access_level() {
        let delete = operations()
            .iter()
            .find(|op| op.access == AccessLevel::Destructive && op.method == HttpMethod::Delete)
            .unwrap();
        let concrete = delete.path.replace("{id}", "42");
        let variants = [
            concrete.clone(),
            concrete.to_uppercase().replacen("/V", "/v", 1),
            format!("{concrete}/"),
            concrete.replacen('a', "%61", 1),
        ];
        for path in variants {
            assert_eq!(
                route(HttpMethod::Delete, &path).access,
                AccessLevel::Destructive,
                "{path}"
            );
        }
    }

    #[test]
    fn unknown_paths_use_the_method() {
        assert_eq!(
            route(HttpMethod::Get, "/v1/nothing").access,
            AccessLevel::Read
        );
        assert_eq!(
            route(HttpMethod::Post, "/v1/nothing").access,
            AccessLevel::Write
        );
        assert_eq!(
            route(HttpMethod::Patch, "/v1/nothing").access,
            AccessLevel::Write
        );
        assert_eq!(
            route(HttpMethod::Delete, "/v1/nothing").access,
            AccessLevel::Destructive
        );
        assert!(route(HttpMethod::Get, "/v1/nothing").matched.is_none());
        // Версия сравнивается точно.
        assert!(route(HttpMethod::Get, "/v9/apps/1").matched.is_none());
    }

    #[test]
    fn every_operation_matches_itself() {
        for op in operations() {
            let concrete = op.path.replace('{', "x").replace('}', "");
            let route = route(op.method, &concrete);
            assert!(route.access >= op.access, "{}", op.tool_name);
            assert!(route.matched.is_some(), "{}", op.tool_name);
        }
    }
}
