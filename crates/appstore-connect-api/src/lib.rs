//! Клиент App Store Connect API.
//!
//! Операции описаны в OpenAPI-спецификации Apple (снимок `spec/openapi.oas.json`)
//! и файле-дополнении с именами инструментов, наборами и уровнями доступа.
//!
//! - [`spec`] — операции API, сгенерированные из спецификации при сборке.
//! - [`executor`] — выполнение HTTP-запросов к API (JWT, повторы, выдача, ошибки JSON:API).
//! - [`external`] — запросы на адреса, выданные API (`uploadOperations`, подписанные ссылки).
//! - [`path`] — проверка и сборка относительных путей API.
//! - [`jwt`] — выпуск JWT (ES256) из ключа `.p8`, только в памяти.
//! - [`key`] — ключ `.p8`, Key ID, Issuer ID: обёртки, которые не попадают в логи и `Debug`.
//! - [`auth`] — хранение ключей в системном хранилище (Keychain и аналоги).

pub mod auth;
pub mod executor;
pub mod external;
pub mod jwt;
pub mod key;
pub mod path;
pub mod spec;

pub use executor::{API_HOST, DEFAULT_BASE_URL};
pub use jwt::{Jwt, JwtTokens};
pub use key::{ApiKey, Issuer, IssuerId, KeyError, KeyId, PrivateKeyPem};
pub use spec::{
    AccessLevel, BodyKind, ExcludedOperationDef, ExclusionReason, HttpMethod, OperationDef,
    ParamDef, ParamLocation, ParamType, ResponseKind, ToolsetDef,
};
