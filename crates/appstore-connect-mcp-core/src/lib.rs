//! Ядро MCP-сервера App Store Connect: регистрация инструментов, уровни доступа,
//! постобработка ответов и инструкции для агентов. Не зависит от транспорта.
//!
//! - [`server`] — обработчик `rmcp` ([`AppStoreServer`]) и инструкции для агента;
//! - [`registry`] — какие инструменты регистрируются при заданных [`ServerOptions`];
//! - [`api_search`] и [`api_request`] — поиск по всем операциям и запрос к любому пути API;
//! - [`routes`] — уровень доступа произвольного запроса по шаблонам операций;
//! - [`request`] — сборка запроса к API из аргументов инструмента операции;
//! - [`validate`] — проверка аргументов составных инструментов по их схеме;
//! - [`response`] — представление ответа API (с постраничной выдачей) в результате инструмента;
//! - [`flatten`] — аргумент `flatten`: связанные объекты из `included` на место ссылок;
//! - [`error`] — ошибки вызова с текстом для агента;
//! - [`toolsets`] — наборы инструментов (`--toolsets`) и набор по умолчанию;
//! - [`files`] — политика доступа к локальным файлам ([`FilePolicy`]);
//! - [`composite`] — составные инструменты и их контракт с сервером;
//! - [`prompts`] — MCP-промпты для частых сценариев;
//! - [`redact`] — вырезание учётных данных, подписанных адресов и большого содержимого;
//! - [`jq`] — фильтр ответа в стиле jq (аргумент `jq`);
//! - [`custom_tools`] — пользовательские инструменты из `tools.toml`;
//! - [`dynamic_toolsets`] — `list_toolsets`, `enable_toolset`, `disable_toolset` (режим
//!   `--dynamic-toolsets`).
//!
//! Транспорт (stdio) и CLI живут в крейте `appstore-connect-mcp`, клиент API —
//! в `appstore-connect-api`.

pub mod api_request;
pub mod api_search;
pub mod composite;
pub mod custom_tools;
pub mod dynamic_toolsets;
pub mod error;
pub mod files;
pub mod flatten;
pub mod jq;
pub mod prompts;
pub mod redact;
pub mod registry;
pub mod request;
pub mod response;
pub mod routes;
pub mod server;
pub mod toolsets;
pub mod validate;

pub use appstore_connect_api;
pub use composite::{CompositeTool, ToolError};
pub use custom_tools::{CustomTool, CustomToolDef, CustomToolsFile};
pub use files::{FilePolicy, FilePolicyError, LOCAL_FILES_DISABLED};
pub use jq::JqRunner;
pub use prompts::PromptDef;
pub use registry::{RegisteredTool, ToolKind, ToolRegistry, access_name};
pub use server::{AppStoreServer, SERVER_NAME, Services};
pub use toolsets::{
    ALL_TOOLSETS, DEFAULT_TOOLSETS, Toolset, available_toolsets, effective_toolsets,
    normalize_toolsets,
};

use appstore_connect_api::AccessLevel;

/// Настройки MCP-сервера.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Максимальный разрешённый уровень доступа. Инструменты выше него не регистрируются.
    pub max_access: AccessLevel,
    /// Включённые наборы инструментов (slug, проверяются [`normalize_toolsets`]).
    /// `None` или пустой список — набор по умолчанию ([`DEFAULT_TOOLSETS`]). При
    /// `dynamic_toolsets` — наборы, включённые при запуске (`None` — ни одного).
    pub toolsets: Option<Vec<String>>,
    /// Динамические наборы: при запуске регистрируются только `api_search`, `api_request`,
    /// инструменты управления наборами и пользовательские инструменты, остальные агент
    /// включает `enable_toolset` (см. [`dynamic_toolsets`]). Уровень доступа действует как
    /// обычно.
    pub dynamic_toolsets: bool,
    /// Как выполняются фильтры `jq` (см. [`jq`]). По умолчанию — в потоке этого процесса;
    /// `serve` выполняет их в отдельном процессе.
    pub jq: JqRunner,
    /// Пользовательские инструменты (см. [`custom_tools`]). Регистрируются те, чей уровень
    /// доступа не выше `max_access`; наборы инструментов их не ограничивают.
    pub custom_tools: Vec<CustomTool>,
    /// Инструменты могут читать и писать локальные файлы (`save_to`, загрузки составных
    /// инструментов). `false` — такие инструменты не регистрируются, а любой путь
    /// отклоняется ([`FilePolicy::disabled`]). Сервер stdio работает на машине агента,
    /// поэтому по умолчанию `true`.
    pub local_files: bool,
    /// Профиль (кабинет), с которым запущен сервер: подставляется в инструкции
    /// (`appstore-connect-mcp auth login --profile …`) и в контекст составных инструментов.
    pub profile: Option<String>,
    /// Номер поставщика (Vendor Number) профиля для отчётов продаж и финансов.
    pub vendor_number: Option<String>,
    /// Составные инструменты, из которых регистрируются подходящие по уровню доступа,
    /// набору и локальным файлам. По умолчанию — [`composite::all`].
    pub composite_tools: Vec<CompositeTool>,
    /// Промпты сервера. По умолчанию — [`prompts::all`].
    pub prompts: Vec<PromptDef>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            max_access: AccessLevel::Read,
            toolsets: None,
            dynamic_toolsets: false,
            jq: JqRunner::default(),
            custom_tools: Vec::new(),
            local_files: true,
            profile: None,
            vendor_number: None,
            composite_tools: composite::all(),
            prompts: prompts::all(),
        }
    }
}

/// Уровень доступа по флагам `--allow-write` и `--allow-destructive`: удаление подразумевает
/// запись. Единое правило для `serve`, статуса и профилей.
pub fn access_from_flags(allow_write: bool, allow_destructive: bool) -> AccessLevel {
    if allow_destructive {
        AccessLevel::Destructive
    } else if allow_write {
        AccessLevel::Write
    } else {
        AccessLevel::Read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_implies_write() {
        assert_eq!(access_from_flags(false, false), AccessLevel::Read);
        assert_eq!(access_from_flags(true, false), AccessLevel::Write);
        assert_eq!(access_from_flags(false, true), AccessLevel::Destructive);
        assert_eq!(access_from_flags(true, true), AccessLevel::Destructive);
    }
}
