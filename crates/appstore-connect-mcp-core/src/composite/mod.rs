//! Составные инструменты: несколько запросов к API (и к адресам, которые выдаёт API)
//! за один вызов MCP-инструмента.
//!
//! Модуль задаёт контракт между составными инструментами и ядром сервера:
//!
//! - каждый инструмент — отдельный модуль с функцией `tools() -> Vec<CompositeTool>`;
//! - [`all`] собирает их в один список, сервер регистрирует его так же, как инструменты
//!   операций: по уровню доступа ([`CompositeTool::access`]), набору
//!   ([`CompositeTool::toolset`]) и признаку локальных файлов;
//! - ответ инструмента проходит ту же постобработку, что и ответы операций (вырезание
//!   секретов, `jq`, лимит размера). Инструмент сам не кладёт в ответ то, чего агенту
//!   видеть не нужно: временные адреса загрузки, содержимое файлов, приватные ключи.
//!
//! Инструменты обращаются к API только через [`Context::api`], к временным адресам Apple —
//! только через [`Context::external`], к локальным файлам — только через [`Context::files`].

use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use appstore_connect_api::{
    AccessLevel,
    executor::{ApiClient, ApiError},
    external::ExternalClient,
};
use serde_json::Value;

pub mod asset_upload;
pub mod build_upload;
pub mod metadata;
pub mod release_status;
pub mod report_download;

mod certificate_create;
mod profile_download;
mod upload;

/// Результат вызова составного инструмента: JSON для агента.
pub type ToolResult = Result<Value, ToolError>;

/// Будущее значение результата; инструменты асинхронные.
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

/// Описание составного инструмента.
#[derive(Clone, Copy)]
pub struct CompositeTool {
    /// Имя MCP-инструмента, не длиннее 40 символов, не совпадает с инструментами операций.
    pub name: &'static str,
    /// Slug набора инструментов (один из наборов спецификации).
    pub toolset: &'static str,
    /// Уровень доступа, задаётся явно.
    pub access: AccessLevel,
    /// Описание для агента (английский): что делает, когда использовать, что вернёт.
    pub description: &'static str,
    /// JSON Schema аргументов: `type: object`, `additionalProperties: false`.
    pub input_schema: fn() -> Value,
    /// Читает или пишет локальные файлы: без локального режима не регистрируется.
    pub uses_local_files: bool,
    /// Выполнение. Аргументы уже проверены сервером по `input_schema`.
    pub run: for<'a> fn(&'a Context, Value) -> ToolFuture<'a>,
}

impl std::fmt::Debug for CompositeTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeTool")
            .field("name", &self.name)
            .field("toolset", &self.toolset)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

/// Окружение вызова: клиенты, файлы, хранилище секретов и данные профиля.
pub struct Context {
    /// Клиент App Store Connect API (JWT добавляет он сам).
    pub api: Arc<ApiClient>,
    /// Клиент для адресов, выданных API (`uploadOperations`, подписанные ссылки отчётов):
    /// без `Authorization`, только https.
    pub external: Arc<ExternalClient>,
    /// Доступ к локальным файлам по правилам сервера.
    pub files: Arc<dyn LocalFiles>,
    /// Хранилище секретов, которые создают инструменты (приватные ключи сертификатов,
    /// пароли `.p12`).
    pub vault: Arc<dyn SecretVault>,
    /// Профиль сервера: имя и настройки, нужные инструментам.
    pub profile: ProfileInfo,
    /// Уровень доступа сервера. Инструмент проверяет его сам, если отдельный режим
    /// вызова требует уровня выше, чем у самого инструмента (например, удаление локалей).
    pub access: AccessLevel,
}

/// Данные профиля, нужные составным инструментам.
#[derive(Debug, Clone, Default)]
pub struct ProfileInfo {
    /// Имя профиля (один профиль — один кабинет).
    pub name: String,
    /// Vendor number для отчётов продаж и финансов.
    pub vendor_number: Option<String>,
}

/// Правила локальных файлов сервера (рабочий каталог, временный каталог, `--allow-path`).
///
/// Реализация ядра проверяет путь, разворачивает символические ссылки и возвращает
/// канонический путь; открывать файл нужно по нему.
pub trait LocalFiles: Send + Sync {
    /// Существующий файл для чтения.
    fn resolve_read(&self, path: &str) -> Result<PathBuf, ToolError>;
    /// Новый файл для записи: родительский каталог существует, сам файл — нет
    /// (существующие файлы не перезаписываются).
    fn resolve_write(&self, path: &str) -> Result<PathBuf, ToolError>;
    /// Каталог для записи нескольких новых файлов (создаётся, если его нет).
    fn resolve_write_dir(&self, path: &str) -> Result<PathBuf, ToolError>;
}

/// Хранилище секретов, которые создают сами инструменты. Значения наружу не отдаются:
/// агент получает только имя записи.
pub trait SecretVault: Send + Sync {
    /// Сохранить значение под именем записи (`account`), заменив прежнее.
    fn store(&self, account: &str, value: &str) -> Result<(), ToolError>;
    /// Есть ли запись.
    fn contains(&self, account: &str) -> Result<bool, ToolError>;
}

/// Ошибка составного инструмента: текст для агента (английский), без секретов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Текст ошибки для агента.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<ApiError> for ToolError {
    /// Текст исполнителя и подсказка для агента ([`crate::error::describe_api_error`]).
    fn from(err: ApiError) -> Self {
        Self::new(crate::error::describe_api_error(&err))
    }
}

/// Все составные инструменты. Новый модуль добавляет сюда свой `tools()`.
pub fn all() -> Vec<CompositeTool> {
    let mut tools = Vec::new();
    tools.extend(asset_upload::tools());
    tools.extend(build_upload::tools());
    tools.extend(certificate_create::tools());
    tools.extend(metadata::tools());
    tools.extend(profile_download::tools());
    tools.extend(release_status::tools());
    tools.extend(report_download::tools());
    tools
}

/// Реализации для тестов составных инструментов.
pub mod testing {
    use std::{
        collections::HashMap,
        path::{Component, Path, PathBuf},
        sync::Mutex,
    };

    use super::{LocalFiles, SecretVault, ToolError};

    /// Файлы только внутри одного каталога (обычно `tempfile::TempDir`).
    pub struct DirFiles {
        root: PathBuf,
    }

    impl DirFiles {
        pub fn new(root: &Path) -> Self {
            Self {
                root: root.to_path_buf(),
            }
        }

        fn inside(&self, path: &str) -> Result<PathBuf, ToolError> {
            let candidate = self.root.join(path);
            let escapes = candidate
                .components()
                .any(|c| matches!(c, Component::ParentDir));
            if escapes || !candidate.starts_with(&self.root) {
                return Err(ToolError::new(format!(
                    "path is outside the allowed directories: {path}"
                )));
            }
            Ok(candidate)
        }
    }

    impl LocalFiles for DirFiles {
        fn resolve_read(&self, path: &str) -> Result<PathBuf, ToolError> {
            let p = self.inside(path)?;
            if !p.is_file() {
                return Err(ToolError::new(format!("file not found: {path}")));
            }
            Ok(p)
        }

        fn resolve_write(&self, path: &str) -> Result<PathBuf, ToolError> {
            let p = self.inside(path)?;
            if p.exists() {
                return Err(ToolError::new(format!("file already exists: {path}")));
            }
            Ok(p)
        }

        fn resolve_write_dir(&self, path: &str) -> Result<PathBuf, ToolError> {
            let p = self.inside(path)?;
            std::fs::create_dir_all(&p).map_err(|e| ToolError::new(e.to_string()))?;
            Ok(p)
        }
    }

    /// Хранилище секретов в памяти.
    #[derive(Default)]
    pub struct MemoryVault {
        items: Mutex<HashMap<String, String>>,
    }

    impl MemoryVault {
        /// Значение записи — только для проверок в тестах.
        pub fn value(&self, account: &str) -> Option<String> {
            self.items.lock().ok()?.get(account).cloned()
        }
    }

    impl SecretVault for MemoryVault {
        fn store(&self, account: &str, value: &str) -> Result<(), ToolError> {
            self.items
                .lock()
                .map_err(|_| ToolError::new("vault lock poisoned"))?
                .insert(account.to_string(), value.to_string());
            Ok(())
        }

        fn contains(&self, account: &str) -> Result<bool, ToolError> {
            Ok(self
                .items
                .lock()
                .map_err(|_| ToolError::new("vault lock poisoned"))?
                .contains_key(account))
        }
    }
}
