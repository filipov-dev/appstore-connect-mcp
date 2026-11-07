//! Уровни доступа операций.
//!
//! По умолчанию уровень задаёт HTTP-метод: `GET` — чтение, `POST`/`PATCH`/`PUT` — запись,
//! `DELETE` — удаление. Удаление связи (`DELETE …/relationships/…`) — запись: сами объекты
//! остаются, связь возвращается обратной операцией. Всё остальное — явные исключения
//! из `spec/overlay.toml` (секция `[[access]]`), они важнее обоих правил.

use std::collections::BTreeMap;

use crate::overlay::{AccessRule, Level};

impl Level {
    pub fn rust_name(self) -> &'static str {
        match self {
            Self::Read => "AccessLevel::Read",
            Self::Write => "AccessLevel::Write",
            Self::Destructive => "AccessLevel::Destructive",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Destructive => "destructive",
        }
    }
}

/// Уровень по методу и пути, без исключений.
pub fn default_access(method: &str, path: &str) -> Level {
    match method {
        "get" => Level::Read,
        "delete" if path.contains("/relationships/") => Level::Write,
        "delete" => Level::Destructive,
        _ => Level::Write,
    }
}

/// Исключения: `operationId` → (уровень, объяснение).
pub struct Exceptions<'a> {
    by_operation: BTreeMap<&'a str, (Level, &'a str)>,
}

impl<'a> Exceptions<'a> {
    pub fn new(rules: &'a [AccessRule]) -> Self {
        let mut by_operation = BTreeMap::new();
        for rule in rules {
            assert!(
                !rule.reason.trim().is_empty(),
                "spec/overlay.toml: у правила доступа нет reason"
            );
            for operation in &rule.operations {
                let previous =
                    by_operation.insert(operation.as_str(), (rule.level, rule.reason.trim()));
                assert!(
                    previous.is_none(),
                    "spec/overlay.toml: операция {operation} указана в [[access]] дважды"
                );
            }
        }
        Self { by_operation }
    }

    pub fn operations(&self) -> impl Iterator<Item = &&'a str> {
        self.by_operation.keys()
    }

    /// Уровень операции и объяснение исключения (`None` — уровень по умолчанию).
    pub fn access_for(
        &self,
        operation_id: &str,
        method: &str,
        path: &str,
    ) -> (Level, Option<&'a str>) {
        let default = default_access(method, path);
        match self.by_operation.get(operation_id) {
            Some((level, reason)) => {
                assert_ne!(
                    *level, default,
                    "spec/overlay.toml: исключение для {operation_id} совпадает с уровнем по умолчанию"
                );
                (*level, Some(*reason))
            }
            None => (default, None),
        }
    }
}
