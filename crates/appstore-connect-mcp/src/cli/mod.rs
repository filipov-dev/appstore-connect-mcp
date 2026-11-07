//! Подкоманды CLI. Каждая подкоманда живёт в своём модуле.

pub mod auth;
pub mod connect;
pub mod jq_worker;
pub mod key;
pub mod serve;
pub mod tools;
pub mod toolsets;

use clap::Args;

use crate::profiles::{DEFAULT_PROFILE, PROFILE_ENV};

/// Имя профиля: `--profile`, иначе `APPSTORE_CONNECT_MCP_PROFILE`, иначе `default`.
#[derive(Debug, Clone, Args)]
pub struct ProfileArg {
    /// Profile name (one profile per App Store Connect account).
    #[arg(long, env = PROFILE_ENV, default_value = DEFAULT_PROFILE)]
    pub profile: String,
}
