//! `appstore-connect-mcp serve` — запуск MCP-сервера через stdio.
//!
//! Пользовательские инструменты — из `tools.toml` рядом с `profiles.toml` и из файлов
//! `--tools-file` ([`crate::cli::tools`]); фильтры `jq` выполняются в дочернем процессе этого
//! бинарника ([`crate::cli::jq_worker`]).
//!
//! stdout занят протоколом MCP, всё остальное (логи, ошибки запуска) — только в stderr.
//! Ошибка запуска (нет ключа, хранилище недоступно, неизвестный набор) возвращается до
//! начала протокола: в stdout ничего не пишется, код выхода ненулевой.
//!
//! Уровень доступа и наборы: флаги командной строки, если заданы, иначе настройки профиля,
//! иначе только чтение и наборы по умолчанию. Ключ — только из системного хранилища по
//! профилю; JWT выпускается лениво, при первом запросе к API, поэтому запуск и
//! `initialize`/`tools/list` обходятся без сети.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use appstore_connect_api::auth::{KEYRING_SERVICE, KeyStore, KeyringStore, KeyringVault};
use appstore_connect_api::executor::ClientConfig;
use appstore_connect_api::external::ExternalClient;
use appstore_connect_api::jwt::JwtTokens;
use appstore_connect_api::{AccessLevel, executor::ApiClient};
use appstore_connect_mcp_core::composite::SecretVault;
use appstore_connect_mcp_core::server::PROCESSING_INPUT_LIMIT;
use appstore_connect_mcp_core::{
    ALL_TOOLSETS, AppStoreServer, FilePolicy, ServerOptions, Services, ToolRegistry,
    access_from_flags, access_name, normalize_toolsets,
};
use clap::Args;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

use crate::cli::{jq_worker, tools};
use crate::profiles::{
    DEFAULT_PROFILE, PROFILE_ENV, ProfileRepo, ProfileSettings, load_profile_with,
    validate_profile_name,
};
use crate::ui::{TerminalUi, Ui};

/// Таймаут запроса к API по умолчанию, секунды.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Минимальный таймаут попытки для адресов загрузки и скачивания файлов, секунды.
pub const MIN_TRANSFER_TIMEOUT_SECS: u64 = 300;
/// Лимит ответа, возвращаемого агенту, по умолчанию: 256 KiB.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Уровень логов в stderr, если не задан ни `--log-level`, ни `RUST_LOG`.
const DEFAULT_LOG_LEVEL: &str = "warn";

#[derive(Debug, Clone, Args)]
#[command(after_help = "\
Access level and toolsets: flags given on the command line win; without them the settings \
saved for the profile are used. Without any, the server is read-only and exposes the default \
toolsets: apps, versions-review, builds, testflight. --allow-destructive implies --allow-write. \
Use --toolsets all for every toolset; `appstore-connect-mcp toolsets` lists them with tool \
counts. Tools above the access level are not registered at all: calling them explains which \
flag enables them.

Dynamic toolsets: with --dynamic-toolsets (or the profile setting) the server starts with only \
`api_search`, `api_request`, `list_toolsets`, `enable_toolset` and `disable_toolset`, plus the \
toolsets given with --toolsets and the custom tools; the agent enables toolsets on demand and \
the client is notified that the tool list changed. Use it with clients that load all tool \
descriptions up front and reload the list on `notifications/tools/list_changed`.

Response filter: operation tools with a JSON response, composite tools, custom tools and \
`api_request` accept an optional `jq` argument (jaq dialect) applied to the JSON response; \
--max-response-bytes then limits the filtered result. Filters run in a child process without \
environment, files or network, with a timeout.

Custom tools: tools defined in tools.toml next to profiles.toml \
(~/.config/appstore-connect-mcp/tools.toml) are registered when the file exists; files from \
other places (for example, a project repository) only with --tools-file. A custom tool calls \
one API operation with fixed arguments and an optional `jq` filter, and has the access level \
of that operation. Check the files with `appstore-connect-mcp tools check`.

API key: read only from the profile (the .p8 key from the system keychain, Key ID and Issuer \
ID from profiles.toml); store it with `appstore-connect-mcp auth login`. The key is never read \
from environment variables or command-line arguments. Short-lived JWTs are signed on the \
first API request, so starting the server needs no network.

Responses: tools return the JSON:API response of App Store Connect. When there is a next page \
or a total count, the response is wrapped as {\"result\", \"pagination\": {\"next\", \"total\", \
\"limit\"}}; when the hourly request quota is almost used up, \"rate_limit\" is added. \
--max-response-bytes limits the text returned to the agent.

Local files: file arguments of tools (`save_to` for reports, files of upload tools) must be \
inside the server's working directory (unless it is the filesystem root or the home \
directory), the system temp directory, or a directory given with --allow-path. The temp \
directory is allowed for scratch files and downloads; other programs share it, so keep nothing \
private there. Paths are checked after resolving `..` and symbolic links: a link that leads \
outside these directories is rejected, and existing files are never overwritten.

Logs go to stderr only (stdout carries the MCP protocol). The level comes from --log-level, \
otherwise from RUST_LOG, otherwise `warn`.")]
pub struct ServeArgs {
    /// Profile with the API key [default: default].
    #[arg(long, env = PROFILE_ENV)]
    pub profile: Option<String>,
    /// Register tools that create or modify data.
    #[arg(long)]
    pub allow_write: bool,
    /// Register tools that delete data (implies --allow-write).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable, or `all` [default: apps,versions-review,builds,testflight].
    /// With --dynamic-toolsets: the toolsets enabled at start [default: none].
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Load toolsets on demand: start with only the search, request and toolset tools, and
    /// let the agent enable toolsets with `enable_toolset`.
    #[arg(long)]
    pub dynamic_toolsets: bool,
    /// Additional custom tools file (same format as ~/.config/appstore-connect-mcp/tools.toml).
    /// Repeatable.
    #[arg(long, value_name = "PATH")]
    pub tools_file: Vec<PathBuf>,
    /// Extra directory that tools may read files from or save files to. Repeatable.
    #[arg(long, value_name = "DIR")]
    pub allow_path: Vec<PathBuf>,
    /// Timeout of one API request, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Maximum size of a tool result returned to the agent; larger results are an error.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    pub max_response_bytes: usize,
    /// Log level or filter for stderr, e.g. `info` or `appstore_connect_mcp_core=debug`.
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,
    /// Print the effective profile, access level and toolsets, then exit without starting
    /// the server.
    #[arg(long)]
    pub status: bool,
}

impl ServeArgs {
    fn profile(&self) -> &str {
        self.profile.as_deref().unwrap_or(DEFAULT_PROFILE)
    }

    /// Проверка аргументов до обращения к хранилищу.
    fn validate(&self) -> anyhow::Result<()> {
        if self.timeout == 0 {
            bail!("--timeout must be greater than 0");
        }
        if self.max_response_bytes == 0 {
            bail!("--max-response-bytes must be greater than 0");
        }
        validate_profile_name(self.profile())
    }
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    init_logging(args.log_level.as_deref())?;
    args.validate()?;
    let repo = ProfileRepo::open_default()?;
    if args.status {
        return status(&KeyringStore, &repo, &args, &mut TerminalUi);
    }
    let setup = Setup::load(&args, &KeyringStore, &repo)?;
    tracing::info!(
        profile = %setup.options.profile.as_deref().unwrap_or_default(),
        access = ?setup.options.max_access,
        toolsets = ?setup.options.toolsets,
        dynamic_toolsets = setup.options.dynamic_toolsets,
        custom_tools = setup.options.custom_tools.len(),
        roots = %setup.files.describe_roots(),
        "запуск MCP-сервера App Store Connect"
    );
    let server = setup.server()?;
    let running = server
        .serve(stdio())
        .await
        .context("MCP initialization over stdio failed")?;
    running.waiting().await.context("MCP server task failed")?;
    Ok(())
}

/// Логи — только в stderr: stdout занят протоколом MCP.
fn init_logging(level: Option<&str>) -> anyhow::Result<()> {
    let filter = match level {
        Some(level) => EnvFilter::try_new(level)
            .map_err(|err| anyhow!("invalid --log-level `{level}`: {err}"))?,
        None => {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_LEVEL))
        }
    };
    // Повторная инициализация (тесты) не ошибка: остаётся первый подписчик.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();
    Ok(())
}

/// Всё, что нужно для запуска сервера: настройки ядра, политика файлов, клиенты и хранилище
/// секретов.
pub struct Setup {
    pub options: ServerOptions,
    pub files: FilePolicy,
    pub config: ClientConfig,
    pub transfer_timeout: Duration,
    pub vault: Arc<dyn SecretVault>,
}

impl Setup {
    /// Читает профиль (ключ — из `store`) и собирает настройки сервера.
    pub fn load(
        args: &ServeArgs,
        store: &dyn KeyStore,
        repo: &ProfileRepo,
    ) -> anyhow::Result<Self> {
        args.validate()?;
        let files = FilePolicy::with_defaults(&args.allow_path).map_err(|err| anyhow!(err))?;
        let profile_name = args.profile();
        let profile = load_profile_with(repo, store, profile_name)
            .with_context(|| format!("cannot load profile `{profile_name}`"))?;
        let mut options = effective_options(args, &profile.settings)?;
        options.custom_tools = tools::load(Some(&tools::user_tools_file(repo)), &args.tools_file)?;
        options.jq = jq_worker::current_runner();
        // Ключ уходит в источник JWT; токен выпускается при первом запросе.
        let tokens = Arc::new(JwtTokens::new(profile.key));
        let mut config = ClientConfig::new(Some(tokens))
            .map_err(|err| anyhow!("cannot configure the App Store Connect API client: {err}"))?;
        config.timeout = Duration::from_secs(args.timeout);
        config.max_response_bytes = args.max_response_bytes;
        Ok(Self {
            options,
            files,
            config,
            transfer_timeout: Duration::from_secs(args.timeout.max(MIN_TRANSFER_TIMEOUT_SECS)),
            vault: Arc::new(KeyringVault::new(profile_name)),
        })
    }

    pub fn server(self) -> anyhow::Result<AppStoreServer> {
        let api = ApiClient::new(self.config)
            .map_err(|err| anyhow!("cannot create the App Store Connect API client: {err}"))?;
        let external = ExternalClient::new(self.transfer_timeout, PROCESSING_INPUT_LIMIT)
            .map_err(|err| anyhow!("cannot create the HTTP client for file transfers: {err}"))?;
        AppStoreServer::new(
            Services {
                api,
                external,
                vault: self.vault,
            },
            self.options,
            self.files,
        )
        .map_err(|err| anyhow!(err))
    }
}

/// Откуда взята настройка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Flags,
    Profile,
    Default,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::Flags => "from command-line flags",
            Self::Profile => "from profile settings",
            Self::Default => "default",
        }
    }
}

/// Уровень доступа: флаги командной строки, если заданы, иначе настройки профиля.
/// В обоих случаях `allow_destructive` подразумевает запись.
fn effective_access(args: &ServeArgs, settings: &ProfileSettings) -> (AccessLevel, Source) {
    if args.allow_write || args.allow_destructive {
        (
            access_from_flags(args.allow_write, args.allow_destructive),
            Source::Flags,
        )
    } else {
        let level = access_from_flags(settings.allow_write, settings.allow_destructive);
        let source = if level == AccessLevel::Read {
            Source::Default
        } else {
            Source::Profile
        };
        (level, source)
    }
}

/// Наборы: `--toolsets`, если задан, иначе из профиля; пусто — набор по умолчанию (`None`).
fn effective_toolsets(
    args: &ServeArgs,
    settings: &ProfileSettings,
) -> anyhow::Result<(Option<Vec<String>>, Source)> {
    let cli = normalize_toolsets(&args.toolsets).map_err(|err| anyhow!(err))?;
    if !cli.is_empty() {
        return Ok((Some(cli), Source::Flags));
    }
    let saved =
        normalize_toolsets(&settings.toolsets).map_err(|err| anyhow!("profile settings: {err}"))?;
    Ok(if saved.is_empty() {
        (None, Source::Default)
    } else {
        (Some(saved), Source::Profile)
    })
}

fn effective_options(
    args: &ServeArgs,
    settings: &ProfileSettings,
) -> anyhow::Result<ServerOptions> {
    let (max_access, _) = effective_access(args, settings);
    let (toolsets, _) = effective_toolsets(args, settings)?;
    Ok(ServerOptions {
        max_access,
        toolsets,
        dynamic_toolsets: args.dynamic_toolsets || settings.dynamic_toolsets,
        local_files: true,
        profile: Some(args.profile().to_owned()),
        vendor_number: settings.vendor_number.clone(),
        ..ServerOptions::default()
    })
}

/// `serve --status`: действующие настройки сервера без запуска и без запросов к API.
/// Ключ не выводится: печатается только, сохранён ли он.
pub fn status(
    store: &dyn KeyStore,
    repo: &ProfileRepo,
    args: &ServeArgs,
    ui: &mut dyn Ui,
) -> anyhow::Result<()> {
    args.validate()?;
    let profile = args.profile();
    let settings = repo.get(profile)?.unwrap_or_default();
    let has_key = store.contains(profile)?;
    let (access, access_source) = effective_access(args, &settings);
    let (toolsets, toolsets_source) = effective_toolsets(args, &settings)?;
    let tools_file = tools::user_tools_file(repo);
    let options = ServerOptions {
        max_access: access,
        toolsets,
        custom_tools: tools::load(Some(&tools_file), &args.tools_file)?,
        ..effective_options(args, &settings)?
    };
    let registry = ToolRegistry::new(&options).map_err(|err| anyhow!(err))?;
    let files = FilePolicy::with_defaults(&args.allow_path).map_err(|err| anyhow!(err))?;

    ui.say(&format!("Profile:   {profile}"));
    ui.say(&format!(
        "Key:       {}",
        if has_key {
            format!("stored in the system keychain (service `{KEYRING_SERVICE}`)")
        } else {
            format!(
                "not found; store it with `appstore-connect-mcp auth login --profile {profile}`"
            )
        }
    ));
    ui.say(&format!(
        "Key ID:    {}",
        settings.key_id.as_deref().unwrap_or("not set")
    ));
    ui.say(&format!(
        "Issuer:    {}",
        if settings.individual {
            "individual key".to_owned()
        } else {
            settings
                .issuer_id
                .clone()
                .unwrap_or_else(|| "not set".to_owned())
        }
    ));
    ui.say(&format!(
        "Access:    {} ({}; {})",
        access_name(access),
        access_description(access),
        access_source.label()
    ));
    let enabled = registry.enabled_toolsets();
    let toolsets_text = if enabled.len() == appstore_connect_api::spec::toolsets().len() {
        ALL_TOOLSETS.to_owned()
    } else if enabled.is_empty() {
        "none".to_owned()
    } else {
        enabled.join(",")
    };
    ui.say(&format!(
        "Toolsets:  {toolsets_text} ({})",
        toolsets_source.label()
    ));
    // В динамическом режиме это наборы при запуске (без выбора — ни одного), остальные
    // агент включает сам.
    let dynamic = match (args.dynamic_toolsets, settings.dynamic_toolsets) {
        (true, _) => format!("on ({})", Source::Flags.label()),
        (false, true) => format!("on ({})", Source::Profile.label()),
        (false, false) => "off".to_owned(),
    };
    ui.say(&format!("Dynamic:   {dynamic}"));
    ui.say(&format!("Tools:     {}", registry.len()));
    let custom_registered = options
        .custom_tools
        .iter()
        .filter(|tool| registry.get(&tool.name).is_some())
        .count();
    ui.say(&format!(
        "Custom:    {} defined, {custom_registered} registered (user file: {}{})",
        options.custom_tools.len(),
        tools_file.display(),
        if tools_file.exists() {
            ""
        } else {
            ", not found"
        }
    ));
    ui.say(&format!("Files:     {}", files.describe_roots()));
    ui.say(&format!("Profiles:  {}", repo.path().display()));
    let configured =
        settings.key_id.is_some() && (settings.individual || settings.issuer_id.is_some());
    if !has_key || !configured {
        ui.say(
            "The server cannot start until the profile has an API key, a Key ID and an Issuer ID \
             (or is marked as an individual key).",
        );
    }
    Ok(())
}

fn access_description(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read tools only",
        AccessLevel::Write => "read and write tools, no delete",
        AccessLevel::Destructive => "read, write and delete tools",
    }
}

#[cfg(test)]
mod tests {
    use appstore_connect_api::PrivateKeyPem;
    use appstore_connect_api::auth::MemoryStore;
    use appstore_connect_mcp_core::composite::testing::MemoryVault;
    use clap::Parser;
    use rmcp::model::CallToolRequestParams;
    use serde_json::json;
    use wiremock::matchers::{header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::testing::generated_key_pem;
    use crate::ui::testing::ScriptedUi;

    const ISSUER: &str = "01234567-89ab-cdef-0123-456789abcdef";

    #[derive(Debug, Parser)]
    struct Wrapper {
        #[command(flatten)]
        serve: ServeArgs,
    }

    fn try_parse(args: &[&str]) -> Result<ServeArgs, clap::Error> {
        let mut argv = vec!["serve"];
        argv.extend_from_slice(args);
        Wrapper::try_parse_from(argv).map(|w| w.serve)
    }

    fn parse(args: &[&str]) -> ServeArgs {
        try_parse(args).unwrap()
    }

    fn settings(write: bool, destructive: bool, toolsets: &[&str]) -> ProfileSettings {
        ProfileSettings {
            key_id: Some("TESTKEY123".into()),
            issuer_id: Some(ISSUER.into()),
            allow_write: write,
            allow_destructive: destructive,
            toolsets: toolsets.iter().map(|s| s.to_string()).collect(),
            ..ProfileSettings::default()
        }
    }

    fn options(args: &[&str], saved: &ProfileSettings) -> ServerOptions {
        effective_options(&parse(args), saved).unwrap()
    }

    /// Временный каталог профилей и хранилище в памяти: настоящий keychain не трогаем.
    fn fixture(
        settings: Option<ProfileSettings>,
        with_key: bool,
    ) -> (tempfile::TempDir, ProfileRepo, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("config"));
        if let Some(settings) = settings {
            repo.upsert("ci", settings).unwrap();
        }
        let store = MemoryStore::new();
        if with_key {
            store
                .set("ci", &PrivateKeyPem::parse(&generated_key_pem()).unwrap())
                .unwrap();
        }
        (dir, repo, store)
    }

    fn status_text(args: &[&str], repo: &ProfileRepo, store: &MemoryStore) -> String {
        let mut ui = ScriptedUi::answering(&[]);
        status(store, repo, &parse(args), &mut ui).unwrap();
        ui.text()
    }

    #[test]
    fn defaults() {
        let args = parse(&[]);
        assert_eq!(args.timeout, DEFAULT_TIMEOUT_SECS);
        assert_eq!(args.max_response_bytes, DEFAULT_MAX_RESPONSE_BYTES);
        let options = options(&[], &ProfileSettings::default());
        assert_eq!(options.max_access, AccessLevel::Read);
        assert_eq!(options.toolsets, None);
        assert!(options.local_files);
        assert!(try_parse(&["--timeout", "0"]).unwrap().validate().is_err());
        assert!(
            try_parse(&["--max-response-bytes", "0"])
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(parse(&["--profile", "bad name"]).validate().is_err());
    }

    #[test]
    fn cli_flags_win_over_profile() {
        let saved = settings(true, false, &["analytics-reports"]);
        let from_profile = options(&[], &saved);
        assert_eq!(from_profile.max_access, AccessLevel::Write);
        assert_eq!(
            from_profile.toolsets,
            Some(vec!["analytics-reports".to_owned()])
        );

        let flags = options(
            &["--allow-destructive", "--toolsets", "apps, builds"],
            &saved,
        );
        assert_eq!(flags.max_access, AccessLevel::Destructive);
        assert_eq!(
            flags.toolsets,
            Some(vec!["apps".to_owned(), "builds".to_owned()])
        );

        let all = options(&["--toolsets", "all"], &saved);
        assert_eq!(
            all.toolsets.unwrap().len(),
            appstore_connect_api::spec::toolsets().len()
        );
        let err = effective_options(&parse(&["--toolsets", "nope"]), &saved).unwrap_err();
        assert!(err.to_string().contains("unknown toolset `nope`"), "{err}");
        let broken = settings(false, false, &["nope"]);
        let err = effective_options(&parse(&[]), &broken).unwrap_err();
        assert!(err.to_string().contains("profile settings"), "{err}");
    }

    #[test]
    fn dynamic_toolsets_from_flag_or_profile() {
        assert!(!options(&[], &ProfileSettings::default()).dynamic_toolsets);
        assert!(options(&["--dynamic-toolsets"], &ProfileSettings::default()).dynamic_toolsets);
        let saved = ProfileSettings {
            dynamic_toolsets: true,
            ..settings(false, false, &["builds"])
        };
        let opts = options(&[], &saved);
        assert!(opts.dynamic_toolsets);
        assert_eq!(opts.toolsets, Some(vec!["builds".to_owned()]));

        let (_dir, repo, store) = fixture(Some(saved), true);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(
            text.contains("Dynamic:   on (from profile settings)"),
            "{text}"
        );
        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(text.contains("Dynamic:   off"), "{text}");
        let text = status_text(&["--profile", "ci", "--dynamic-toolsets"], &repo, &store);
        assert!(
            text.contains("Dynamic:   on (from command-line flags)"),
            "{text}"
        );
        assert!(text.contains("Toolsets:  none"), "{text}");
        assert!(text.contains("Tools:     5"), "{text}");
    }

    #[test]
    fn status_counts_custom_tools() {
        let (dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(
            text.contains("Custom:    0 defined, 0 registered (user file: "),
            "{text}"
        );
        assert!(text.contains("tools.toml, not found)"), "{text}");

        std::fs::write(
            dir.path().join("config").join("tools.toml"),
            crate::cli::tools::tests::SAMPLE,
        )
        .unwrap();
        let text = status_text(&["--profile", "ci"], &repo, &store);
        // Инструмент записи определён, но без --allow-write не зарегистрирован.
        assert!(
            text.contains("Custom:    2 defined, 1 registered"),
            "{text}"
        );
        let text = status_text(&["--profile", "ci", "--allow-write"], &repo, &store);
        assert!(
            text.contains("Custom:    2 defined, 2 registered"),
            "{text}"
        );

        // `--tools-file` добавляет файл; ошибка в нём — ошибка запуска.
        let extra = dir.path().join("project.toml");
        std::fs::write(&extra, "[[tools]]\nname = \"x\"\n").unwrap();
        let mut ui = ScriptedUi::answering(&[]);
        let args = parse(&["--profile", "ci", "--tools-file", extra.to_str().unwrap()]);
        let err = status(&store, &repo, &args, &mut ui).unwrap_err();
        assert!(err.to_string().contains("project.toml"), "{err}");
    }

    #[test]
    fn status_shows_settings_without_key_values() {
        let (_dir, repo, store) = fixture(Some(settings(false, true, &[])), true);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(text.contains("Profile:   ci"), "{text}");
        assert!(text.contains("stored in the system keychain"), "{text}");
        assert!(text.contains("Key ID:    TESTKEY123"), "{text}");
        assert!(text.contains("Access:    destructive"), "{text}");
        assert!(text.contains("from profile settings"), "{text}");
        assert!(
            text.contains("Toolsets:  apps,versions-review,builds,testflight (default)"),
            "{text}"
        );
        assert!(!text.contains("cannot start"), "{text}");
        assert!(!text.contains("PRIVATE KEY"), "{text}");

        let (_dir, repo, store) = fixture(None, false);
        let text = status_text(&["--profile", "ci", "--toolsets", "all"], &repo, &store);
        assert!(text.contains("Key:       not found"), "{text}");
        assert!(text.contains("Toolsets:  all"), "{text}");
        assert!(text.contains("cannot start"), "{text}");
    }

    #[test]
    fn setup_requires_key_and_ids() {
        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), false);
        let err = Setup::load(&parse(&["--profile", "ci"]), &store, &repo)
            .err()
            .unwrap();
        let text = format!("{err:#}");
        assert!(text.contains("auth login --profile ci"), "{text}");

        let (_dir, repo, store) = fixture(
            Some(ProfileSettings {
                key_id: None,
                ..settings(false, false, &[])
            }),
            true,
        );
        let err = Setup::load(&parse(&["--profile", "ci"]), &store, &repo)
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("no Key ID"), "{err:#}");
    }

    #[tokio::test]
    async fn serves_tools_from_profile() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/apps"))
            .and(header_regex(
                "authorization",
                r"^Bearer eyJ[\w-]+\.[\w-]+\.[\w-]+$",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"type": "apps", "id": "1", "attributes": {"name": "Demo"}}],
                "links": {"self": "x"}
            })))
            .expect(1)
            .mount(&api)
            .await;

        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo).unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        setup.vault = Arc::new(MemoryVault::default());
        let server = setup.server().unwrap();

        let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
        let handle = tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        let client = ().serve(client_io).await.unwrap();

        let info = client.peer_info().unwrap();
        let instructions = info.instructions.as_deref().unwrap();
        assert!(
            instructions.contains("auth login --profile ci"),
            "{instructions}"
        );
        let tools = client.list_all_tools().await.unwrap();
        assert!(tools.iter().any(|t| t.name == "list_apps"));
        assert!(
            tools
                .iter()
                .all(|t| t.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true))
        );

        let result = client
            .call_tool(
                CallToolRequestParams::new("list_apps").with_arguments(
                    json!({"fields.apps": ["name"]})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");

        client.cancel().await.unwrap();
        handle.await.unwrap();
    }
}
