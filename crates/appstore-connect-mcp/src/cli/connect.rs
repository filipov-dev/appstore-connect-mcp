//! `appstore-connect-mcp connect` — подключение сервера к MCP-клиенту одной командой.
//!
//! Сценарий: ключ профиля (сохранённый рабочий ключ используется как есть; иначе —
//! страница ключей App Store Connect с подсказкой, путь к `.p8`, Key ID, Issuer ID,
//! проверка `GET /v1/apps?limit=1`, ключ в системное хранилище) → настройки в
//! `profiles.toml` (права — только если заданы флаги; на macOS повышение прав одобряется
//! Touch ID или паролем) → запись сервера в конфигурацию клиента после подтверждения, без
//! флагов доступа → в конце, один раз, предложение удалить исходный файл ключа.
//!
//! Ключ не проходит через агента и не попадает ни в аргументы, ни в конфигурацию клиента:
//! клиент получает только команду `appstore-connect-mcp serve --profile <профиль> ...`.

use std::path::PathBuf;

use anyhow::{Context, bail};
use appstore_connect_api::ApiKey;
use appstore_connect_api::auth::{AuthError, KeyStore, KeyringStore};
use appstore_connect_api::spec::{find_toolset, toolsets};
use clap::Args;
use mcp_kit::approval::{self, ApprovalStore, Presence};
use mcp_kit::{AccessLevel, ProfileAccess};

use super::ProfileArg;
use crate::clients::claude::{self, CommandRunner, SystemRunner};
use crate::clients::{
    ClientKind, EXE_STEM, Scope, ServerSpec, codex, default_server_name, long_name_warning,
};
use crate::login::{
    Guide, KeyInput, NewKey, offer_to_delete_key_file, offer_vendor_number, remember_key,
    resolve_issuer, resolve_stored_key_id, store_new_key,
};
use crate::profiles::{
    ProfileRepo, ProfileSettings, validate_profile_name, validate_vendor_number,
};
use crate::ui::{SilentUi, TerminalUi, Ui};
use crate::verify::{ApiVerifier, KeyVerifier};

pub use crate::login::API_KEYS_URL;

const CONNECT_HELP: &str = "\
The server reads the API key from the system keychain by profile name. If the profile has no \
stored key yet, connect walks you through getting one step by step: what the key file, the Key \
ID and the Issuer ID are, how to create a team key (the App Store Connect API keys page opens \
in your browser), where to find the downloaded AuthKey_<KEYID>.p8 and the Issuer ID, and, \
optionally, the vendor number for reports. The key is \
checked with GET /v1/apps?limit=1 and stored only in the keychain. If the profile already has \
a working key, it is reused: re-running connect only changes the settings and the client \
registration. Pass --key-file to replace the stored key.

The client config gets only the command `appstore-connect-mcp serve --profile <profile> ...` \
without access flags; never the key. The access is a setting of the profile: --allow-write and \
--allow-destructive change it (without them the profile keeps its access, read-only for a new \
profile), and `appstore-connect-mcp ui` changes it later. On macOS, raising the access asks \
for Touch ID or the macOS password; access that is not approved does not take effect.

One profile is one App Store Connect account. The server name in the client is \
appstore-connect for the default profile and asc-<profile> for others, so that tool names \
mcp__<server>__<tool> stay within the 64-character limit of clients.

With --no-input (alias --silent) nothing is asked and no browser is opened: a new key needs \
--key-file and --issuer-id or --individual (unless saved in the profile), and changing the \
client config needs --yes.

Links open in the default browser; over SSH, without a graphical session or with --no-browser \
they are only printed.";

#[derive(Debug, Clone, Args)]
#[command(after_help = CONNECT_HELP)]
pub struct ConnectArgs {
    /// MCP client to register the server in.
    #[arg(value_enum)]
    pub client: ClientKind,
    #[command(flatten)]
    pub profile: ProfileArg,
    /// MCP server name in the client config. Default: appstore-connect for the default
    /// profile, asc-<profile> for others.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Where to register the server in the client. Codex supports only `user`.
    #[arg(long, value_enum, default_value_t = Scope::User)]
    pub scope: Scope,
    /// Path to the API key file downloaded from App Store Connect (AuthKey_<KEYID>.p8).
    /// Replaces the stored key of the profile.
    #[arg(long, value_name = "PATH")]
    pub key_file: Option<PathBuf>,
    /// Key ID of the key. Default: the KEYID part of the file name AuthKey_<KEYID>.p8, or
    /// the one saved in the profile.
    #[arg(long, value_name = "ID")]
    pub key_id: Option<String>,
    /// Issuer ID of a team key (Users and Access > Integrations > App Store Connect API).
    /// Default: the one saved in the profile.
    #[arg(long, value_name = "UUID", conflicts_with = "individual")]
    pub issuer_id: Option<String>,
    /// The key is an individual key (no Issuer ID).
    #[arg(long)]
    pub individual: bool,
    /// Vendor number for sales and finance reports (Payments and Financial Reports).
    #[arg(long, value_name = "NUMBER")]
    pub vendor_number: Option<String>,
    /// Set the access of the profile to write: tools that create or modify data in App Store
    /// Connect.
    #[arg(long)]
    pub allow_write: bool,
    /// Set the access of the profile to destructive: also tools with irreversible or
    /// user-visible effects: deletions, submissions for review, releases, prices, users and
    /// roles (implies --allow-write).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable (passed to `serve --toolsets`). Default: the
    /// default set of `serve`.
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Register the server without --dynamic-toolsets: all enabled tools are listed at
    /// start instead of being loaded by the agent on demand.
    #[arg(long)]
    pub no_dynamic_toolsets: bool,
    /// Do not ask for confirmation before changing the client config.
    ///
    /// An entry with the same name that runs another server or another profile is never
    /// replaced with --yes.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Do not check the key against the App Store Connect API.
    #[arg(long)]
    pub no_verify: bool,
    /// Do not open links in a browser; only print them.
    #[arg(long)]
    pub no_browser: bool,
    /// Do not ask anything and do not open a browser: take every value from flags and the
    /// profile. Requires --yes.
    #[arg(long, visible_alias = "silent")]
    pub no_input: bool,
}

impl ConnectArgs {
    fn key_input(&self) -> KeyInput<'_> {
        KeyInput {
            key_file: self.key_file.as_deref(),
            key_id: self.key_id.as_deref(),
            issuer_id: self.issuer_id.as_deref(),
            individual: self.individual,
        }
    }
}

pub async fn run(args: ConnectArgs) -> anyhow::Result<()> {
    let exe =
        std::env::current_exe().context("cannot determine the path of appstore-connect-mcp")?;
    let codex_home = match args.client {
        ClientKind::Codex => codex::default_codex_home()?,
        ClientKind::ClaudeCode => PathBuf::new(),
    };
    let verifier = (!args.no_verify).then(ApiVerifier::new).transpose()?;
    let approvals = approval::platform_store(EXE_STEM);
    let presence = approval::platform_presence();
    let mut env = ConnectEnv {
        approval: approvals.as_deref().zip(presence.as_deref()),
        store: &KeyringStore,
        repo: &ProfileRepo::open_default()?,
        verifier: verifier.as_ref(),
        runner: &mut SystemRunner,
        ui: &mut TerminalUi,
        exe,
        codex_home,
        guide: Guide::detect(args.no_browser),
    };
    connect(&mut env, &args).await
}

/// Зависимости `connect`; в тестах подменяются хранилищем в памяти и временными каталогами.
pub struct ConnectEnv<'a, V> {
    /// Одобрение прав (macOS); `None` — платформа без одобрения.
    pub approval: Option<(&'a dyn ApprovalStore, &'a dyn Presence)>,
    pub store: &'a dyn KeyStore,
    pub repo: &'a ProfileRepo,
    /// Проверка ключа; `None` — `--no-verify`.
    pub verifier: Option<&'a V>,
    pub runner: &'a mut dyn CommandRunner,
    pub ui: &'a mut dyn Ui,
    pub exe: PathBuf,
    pub codex_home: PathBuf,
    /// Браузер и каталог загрузок для пошагового мастера (решается в `run`).
    pub guide: Guide,
}

/// Наборы инструментов из флага: без пустых и повторов. Пока генератор операций не
/// заполнил таблицу наборов, проверяется только вид slug; потом — что набор существует.
fn normalize_toolsets(values: &[String]) -> anyhow::Result<Vec<String>> {
    let mut result: Vec<String> = Vec::new();
    for value in values {
        let slug = value.trim();
        if slug.is_empty() || result.iter().any(|known| known == slug) {
            continue;
        }
        let valid_chars = slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
        if !valid_chars {
            bail!("invalid toolset `{slug}`: use lowercase letters, digits, `_` and `-`");
        }
        if !toolsets().is_empty() && find_toolset(slug).is_none() {
            let known: Vec<&str> = toolsets().iter().map(|toolset| toolset.slug).collect();
            bail!("unknown toolset `{slug}`; available: {}", known.join(", "));
        }
        result.push(slug.to_owned());
    }
    Ok(result)
}

pub async fn connect<V: KeyVerifier>(
    env: &mut ConnectEnv<'_, V>,
    args: &ConnectArgs,
) -> anyhow::Result<()> {
    let client = args.client;
    let profile = args.profile.profile.as_str();
    validate_profile_name(profile)?;

    // Всё, что можно проверить без вопросов, — до первого вопроса пользователю.
    let input = args.key_input();
    input.validate()?;
    let vendor_number = args
        .vendor_number
        .as_deref()
        .map(validate_vendor_number)
        .transpose()?;
    if args.no_input && !args.yes {
        bail!("--no-input cannot ask for confirmation; add --yes to change the client config");
    }
    let toolsets = normalize_toolsets(&args.toolsets)?;
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| default_server_name(profile));
    let requested_level = AccessLevel::requested_by_flags(args.allow_write, args.allow_destructive);
    let dynamic_toolsets = !args.no_dynamic_toolsets;
    let spec = ServerSpec::new(
        &name,
        &env.exe,
        profile,
        &ProfileSettings {
            toolsets: toolsets.clone(),
            dynamic_toolsets,
            ..Default::default()
        },
    )?;
    let approval = env.approval;
    let mut settings = env.repo.get(profile)?.unwrap_or_default();
    match client {
        ClientKind::ClaudeCode => claude::ensure_available(env.runner)?,
        ClientKind::Codex => codex::ensure_scope(args.scope)?,
    }
    if let Some(warning) = long_name_warning(&name) {
        env.ui.say(&warning);
    }

    // Тихий режим: тот же сценарий, но без вопросов, браузера и пояснений.
    let mut silent;
    let ui: &mut dyn Ui = if args.no_input {
        env.guide = Guide::none();
        silent = SilentUi::new(&mut *env.ui);
        &mut silent
    } else {
        &mut *env.ui
    };
    let new_key = profile_key(
        env.store,
        env.verifier,
        ui,
        &env.guide,
        &input,
        profile,
        &mut settings,
    )
    .await?;

    if vendor_number.is_some() {
        settings.vendor_number = vendor_number;
    } else if new_key.as_ref().is_some_and(|key| key.guided) {
        offer_vendor_number(ui, &env.guide, &mut settings);
    }
    settings.client = Some(client.name().to_owned());
    if let Some(level) = requested_level {
        change_access(ui, approval, profile, &mut settings, level);
    }
    settings.toolsets = toolsets;
    settings.dynamic_toolsets = dynamic_toolsets;
    env.repo.upsert(profile, settings)?;
    ui.say(&format!(
        "Profile `{profile}` saved to {}.",
        env.repo.path().display()
    ));

    let registered = match client {
        ClientKind::ClaudeCode => claude::register(env.runner, ui, args.scope, &spec, args.yes)?,
        ClientKind::Codex => codex::register(ui, &env.codex_home, &spec, args.yes)?,
    };
    if registered {
        ui.say(&format!(
            "Done: {} now runs `{}` as MCP server `{}`. Restart the client to pick it up.",
            client.display_name(),
            spec.display(),
            spec.name
        ));
    } else {
        ui.say(&format!(
            "{} config was not changed. The key and the profile are kept; re-run \
             `appstore-connect-mcp connect {} --profile {profile}` to register later.",
            client.display_name(),
            client.name()
        ));
    }

    // Один раз за запуск и только для файла, прочитанного в этом запуске.
    if let Some(new_key) = new_key {
        offer_to_delete_key_file(ui, &new_key.file);
    }
    Ok(())
}

/// Меняет уровень доступа профиля (каталоги профиля остаются). На macOS повышение сначала
/// одобряется владельцем; отказ не останавливает `connect`: профиль сохраняет прежние права.
fn change_access(
    ui: &mut dyn Ui,
    approval: Option<(&dyn ApprovalStore, &dyn Presence)>,
    profile: &str,
    settings: &mut ProfileSettings,
    level: AccessLevel,
) {
    let before = settings.access();
    let access = ProfileAccess::new(level, before.allow_path.clone());
    let what = format!("{EXE_STEM} profile \"{profile}\"");
    match approval::save_access_with(approval, profile, &what, &access, |access| {
        settings.set_access(access);
        Ok(())
    }) {
        Ok(saved) if saved.prompted => ui.say(&format!(
            "Access `{}` approved on this Mac.",
            saved.access.summary()
        )),
        Ok(_) => {}
        Err(err) => ui.say(&format!(
            "The access was not changed: {err}. The profile keeps `{}` access; change it later \
             with `appstore-connect-mcp ui` (profile `{profile}`).",
            before.summary()
        )),
    }
}

/// Ключ профиля: рабочий сохранённый используется как есть, иначе принимается новый.
/// Записывает Key ID и тип ключа в `settings`. Возвращает новый ключ, если он был прочитан
/// из файла в этом запуске.
async fn profile_key<V: KeyVerifier>(
    store: &dyn KeyStore,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    guide: &Guide,
    input: &KeyInput<'_>,
    profile: &str,
    settings: &mut ProfileSettings,
) -> anyhow::Result<Option<NewKey>> {
    let stored = match store.get(profile) {
        Ok(pem) => Some(pem),
        Err(AuthError::NotFound { .. }) => None,
        Err(AuthError::Corrupted { .. }) => {
            ui.say(&format!(
                "The value stored for profile `{profile}` is not a valid API key; a new key is \
                 needed."
            ));
            None
        }
        Err(err) => return Err(err.into()),
    };

    match (stored, input.key_file) {
        (Some(_), Some(_)) => ui.say(&format!(
            "Profile `{profile}` already has an API key in the system keychain; it will be \
             replaced with the key from --key-file."
        )),
        (Some(pem), None) => {
            let key_id = resolve_stored_key_id(ui, input, settings)?;
            let issuer = resolve_issuer(ui, input, settings)?;
            let key = ApiKey::new(&pem, key_id, issuer)?;
            ui.say(&format!(
                "Profile `{profile}` already has an API key in the system keychain (Key ID {}).",
                key.key_id().as_str()
            ));
            match verifier {
                None => {
                    ui.say("Keeping the stored key (not checked: --no-verify).");
                    remember_key(settings, &key);
                    return Ok(None);
                }
                Some(verifier) => match verifier.check(&key).await {
                    Ok(outcome) => {
                        ui.say(&format!("The stored key works: {outcome}. Keeping it."));
                        remember_key(settings, &key);
                        return Ok(None);
                    }
                    Err(err) => ui.say(&format!(
                        "The stored key does not work ({err:#}); a new key is needed."
                    )),
                },
            }
        }
        (None, _) => {}
    }

    if input.key_file.is_none() && !ui.is_interactive() {
        bail!(
            "profile `{profile}` has no working API key; pass --key-file \
             <AuthKey_XXXXXXXXXX.p8> (create a key at {API_KEYS_URL})"
        );
    }
    let new_key = store_new_key(store, verifier, ui, guide, profile, input, settings).await?;
    remember_key(settings, &new_key.key);
    Ok(Some(new_key))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use appstore_connect_api::PrivateKeyPem;
    use appstore_connect_api::auth::MemoryStore;
    use mcp_kit::approval::testing::{FakePresence, MemoryStore as MemoryApprovals};

    use super::*;
    use crate::browser::{BrowserDecision, SkipReason};
    use crate::clients::TEST_EXE;
    use crate::clients::claude::testing::FakeRunner;
    use crate::profiles::DEFAULT_PROFILE;
    use crate::testing::generated_key_pem;
    use crate::ui::testing::ScriptedUi;
    use crate::verify::testing::FakeVerifier;

    const KEY_ID: &str = "TESTKEY123";
    const ISSUER: &str = "01234567-89ab-cdef-0123-456789abcdef";

    struct Fixture {
        dir: tempfile::TempDir,
        store: MemoryStore,
        repo: ProfileRepo,
        verifier: FakeVerifier,
        no_verify: bool,
        runner: FakeRunner,
        ui: ScriptedUi,
        codex_home: PathBuf,
        browser: BrowserDecision,
        /// Каталог загрузок мастера; `None` — файл не ищется.
        downloads: Option<PathBuf>,
        /// Одобрение прав, как на macOS; `None` — как на Linux и Windows.
        approval: Option<(MemoryApprovals, FakePresence)>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            Self {
                store: MemoryStore::new(),
                repo: ProfileRepo::in_dir(dir.path().join("config")),
                verifier: FakeVerifier {
                    valid_key_id: KEY_ID.into(),
                },
                no_verify: false,
                runner: FakeRunner::default(),
                ui: ScriptedUi::default(),
                codex_home: dir.path().join("codex"),
                browser: BrowserDecision::Skip(SkipReason::Flag),
                downloads: None,
                approval: None,
                dir,
            }
        }

        /// Терминал с ответами `values`.
        fn answers(&mut self, values: &[&str]) {
            self.ui = ScriptedUi::answering(values);
        }

        /// Файл ключа `name` со сгенерированным ключом; возвращает путь и PEM.
        fn key_file(&self, name: &str) -> (PathBuf, String) {
            let pem = generated_key_pem();
            let path = self.dir.path().join(name);
            fs::write(&path, &pem).unwrap();
            (path, pem)
        }

        /// Профиль с уже сохранённым ключом (как после `auth login`).
        fn with_stored_key(self, profile: &str) -> Self {
            self.store
                .set(
                    profile,
                    &PrivateKeyPem::parse(&generated_key_pem()).unwrap(),
                )
                .unwrap();
            self.repo
                .upsert(
                    profile,
                    ProfileSettings {
                        key_id: Some(KEY_ID.into()),
                        issuer_id: Some(ISSUER.into()),
                        vendor_number: Some("80000001".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            self
        }

        async fn connect(&mut self, args: &ConnectArgs) -> anyhow::Result<()> {
            let approval = self
                .approval
                .as_ref()
                .map(|(store, presence)| (store as &dyn ApprovalStore, presence as &dyn Presence));
            let mut env = ConnectEnv {
                approval,
                store: &self.store,
                repo: &self.repo,
                verifier: (!self.no_verify).then_some(&self.verifier),
                runner: &mut self.runner,
                ui: &mut self.ui,
                exe: PathBuf::from(TEST_EXE),
                codex_home: self.codex_home.clone(),
                guide: Guide {
                    browser: self.browser,
                    downloads: self.downloads.clone(),
                },
            };
            connect(&mut env, args).await
        }

        fn codex_config(&self) -> Option<String> {
            fs::read_to_string(codex::config_path(&self.codex_home)).ok()
        }

        fn settings(&self, profile: &str) -> ProfileSettings {
            self.repo.get(profile).unwrap().unwrap()
        }
    }

    fn args(client: ClientKind) -> ConnectArgs {
        ConnectArgs {
            client,
            profile: ProfileArg {
                profile: DEFAULT_PROFILE.into(),
            },
            name: None,
            scope: Scope::User,
            key_file: None,
            key_id: None,
            issuer_id: None,
            individual: false,
            vendor_number: None,
            allow_write: false,
            allow_destructive: false,
            toolsets: Vec::new(),
            no_dynamic_toolsets: false,
            yes: false,
            no_verify: false,
            no_browser: false,
            no_input: false,
        }
    }

    /// Аргументы `serve` в TOML-записи Codex.
    fn toml_args(args: &[&str]) -> String {
        format!("args = {args:?}")
    }

    fn with_key_file(mut args: ConnectArgs, path: &Path) -> ConnectArgs {
        args.key_file = Some(path.to_path_buf());
        args
    }

    #[tokio::test]
    async fn first_codex_run_opens_the_keys_page_and_stores_key_and_config() {
        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Open;
        let (key_file, pem) = fx.key_file("AuthKey_TESTKEY123.p8");
        let quoted = format!("'{}'", key_file.display());
        // Путь к ключу, Issuer ID, запись конфига — да, удалить файл — нет.
        fx.answers(&[&quoted, ISSUER, "y", "n"]);
        let mut args = args(ClientKind::Codex);
        args.allow_write = true;
        args.toolsets = vec!["apps".into(), " ".into(), "apps".into()];
        args.vendor_number = Some("80000002".into());

        fx.connect(&args).await.unwrap();

        assert_eq!(fx.ui.opened, [API_KEYS_URL]);
        assert_eq!(fx.store.get(DEFAULT_PROFILE).unwrap().expose(), pem);
        let settings = fx.settings(DEFAULT_PROFILE);
        assert_eq!(settings.key_id.as_deref(), Some(KEY_ID));
        assert_eq!(settings.issuer_id.as_deref(), Some(ISSUER));
        assert_eq!(settings.vendor_number.as_deref(), Some("80000002"));
        assert_eq!(settings.client.as_deref(), Some("codex"));
        assert!(settings.allow_write);
        assert!(!settings.allow_destructive);
        assert_eq!(settings.toolsets, ["apps"]);
        assert!(settings.dynamic_toolsets);

        let config = fx.codex_config().unwrap();
        assert!(
            config.contains("[mcp_servers.appstore-connect]"),
            "{config}"
        );
        assert!(
            config.contains(&toml_args(&[
                "serve",
                "--profile",
                "default",
                "--toolsets",
                "apps",
                "--dynamic-toolsets"
            ])),
            "{config}"
        );
        assert_eq!(
            fx.ui.questions.len(),
            4,
            "the key file deletion is offered once: {:?}",
            fx.ui.questions
        );
        assert!(fx.ui.questions[3].contains("Delete the key file"));
        assert_eq!(
            fx.ui
                .questions
                .iter()
                .filter(|q| q.contains("Delete the key file"))
                .count(),
            1
        );
        assert!(key_file.exists());
        let output = fx.ui.text();
        assert!(output.contains("Admin role"), "{output}");
        assert!(
            output.contains("Key ID: TESTKEY123 (from the file name)"),
            "{output}"
        );
        assert!(output.contains("Key is valid"), "{output}");
        // Ключ не выводится и не попадает ни в конфиг клиента, ни в профили.
        let secret_line = pem.lines().nth(1).unwrap();
        assert!(!output.contains(secret_line));
        assert!(!config.contains(secret_line));
        assert!(!config.contains("PRIVATE KEY"));
        let profiles = fs::read_to_string(fx.repo.path()).unwrap();
        assert!(!profiles.contains("PRIVATE KEY"), "{profiles}");
        assert!(fx.runner.calls.is_empty());
    }

    #[tokio::test]
    async fn rerun_reuses_working_key_and_changes_only_settings() {
        let mut fx = Fixture::new().with_stored_key(DEFAULT_PROFILE);
        let before = fx.store.get(DEFAULT_PROFILE).unwrap();
        fx.answers(&["y"]);
        let mut args = args(ClientKind::Codex);
        args.allow_destructive = true;
        args.no_dynamic_toolsets = true;
        fx.connect(&args).await.unwrap();

        // Ни пути к ключу, ни удаления файла: только запись конфига.
        assert_eq!(fx.ui.questions, ["Write the Codex config?"]);
        assert!(fx.ui.opened.is_empty());
        let text = fx.ui.text();
        assert!(text.contains("already has an API key"), "{text}");
        assert!(text.contains("The stored key works"), "{text}");
        assert_eq!(fx.store.get(DEFAULT_PROFILE).unwrap(), before);
        let settings = fx.settings(DEFAULT_PROFILE);
        assert!(settings.allow_write);
        assert!(settings.allow_destructive);
        assert!(!settings.dynamic_toolsets);
        // Несвязанные настройки профиля сохраняются.
        assert_eq!(settings.vendor_number.as_deref(), Some("80000001"));
        assert!(fx.codex_config().unwrap().contains(&toml_args(&[
            "serve",
            "--profile",
            "default"
        ])));

        // Ещё раз с --yes: без вопросов, конфиг не меняется.
        args.yes = true;
        fx.ui = ScriptedUi::default();
        fx.connect(&args).await.unwrap();
        assert!(fx.ui.questions.is_empty());
        assert!(
            fx.ui.text().contains("already up to date"),
            "{}",
            fx.ui.text()
        );
    }

    #[tokio::test]
    async fn broken_stored_key_is_replaced() {
        let mut fx = Fixture::new().with_stored_key(DEFAULT_PROFILE);
        // Профиль указывает на другой Key ID: проверка не проходит.
        let mut settings = fx.settings(DEFAULT_PROFILE);
        settings.key_id = Some("OTHERKEY99".into());
        fx.repo.upsert(DEFAULT_PROFILE, settings).unwrap();

        let (key_file, pem) = fx.key_file("AuthKey_TESTKEY123.p8");
        fx.answers(&[&key_file.display().to_string(), "y", ""]);
        fx.connect(&args(ClientKind::Codex)).await.unwrap();
        let text = fx.ui.text();
        assert!(text.contains("does not work"), "{text}");
        assert!(text.contains("API key replaced"), "{text}");
        assert_eq!(fx.store.get(DEFAULT_PROFILE).unwrap().expose(), pem);
        assert_eq!(fx.settings(DEFAULT_PROFILE).key_id.as_deref(), Some(KEY_ID));
        // Issuer ID — из профиля, файл ключа оставлен (ответ по умолчанию).
        assert!(text.contains("(from the profile)"), "{text}");
        assert!(key_file.exists());
    }

    #[tokio::test]
    async fn key_file_flag_replaces_stored_key_without_questions() {
        let mut fx = Fixture::new().with_stored_key("work");
        let (key_file, pem) = fx.key_file("AuthKey_TESTKEY123.p8");
        let mut args = with_key_file(args(ClientKind::Codex), &key_file);
        args.profile.profile = "work".into();
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert!(fx.ui.questions.is_empty());
        assert_eq!(fx.store.get("work").unwrap().expose(), pem);
        // Без терминала файл ключа не трогается.
        assert!(key_file.exists());
        let config = fx.codex_config().unwrap();
        assert!(config.contains("[mcp_servers.asc-work]"), "{config}");
        assert!(
            config.contains(&toml_args(&[
                "serve",
                "--profile",
                "work",
                "--dynamic-toolsets"
            ])),
            "{config}"
        );
    }

    #[tokio::test]
    async fn non_interactive_run_without_key_explains_what_to_pass() {
        let mut fx = Fixture::new();
        let err = fx
            .connect(&args(ClientKind::Codex))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--key-file"), "{err}");
        assert!(err.contains(API_KEYS_URL), "{err}");
        assert!(fx.codex_config().is_none());
        assert_eq!(fx.repo.get(DEFAULT_PROFILE).unwrap(), None);
    }

    #[tokio::test]
    async fn invalid_key_is_not_stored_and_client_is_not_changed() {
        let mut fx = Fixture::new();
        let (key_file, _) = fx.key_file("AuthKey_WRONGKEY99.p8");
        let mut args = with_key_file(args(ClientKind::Codex), &key_file);
        args.issuer_id = Some(ISSUER.into());
        args.yes = true;
        let err = fx.connect(&args).await.unwrap_err().to_string();
        assert!(err.contains("rejected the key"), "{err}");
        assert!(err.contains("Nothing was stored"), "{err}");
        assert!(!fx.store.contains(DEFAULT_PROFILE).unwrap());
        assert!(fx.codex_config().is_none());
        assert_eq!(fx.repo.get(DEFAULT_PROFILE).unwrap(), None);
    }

    #[tokio::test]
    async fn no_verify_stores_unchecked_key() {
        let mut fx = Fixture::new();
        fx.no_verify = true;
        let (key_file, _) = fx.key_file("AuthKey_WRONGKEY99.p8");
        let mut args = with_key_file(args(ClientKind::Codex), &key_file);
        args.individual = true;
        args.no_verify = true;
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert!(fx.store.contains(DEFAULT_PROFILE).unwrap());
        let settings = fx.settings(DEFAULT_PROFILE);
        assert!(settings.individual);
        assert_eq!(settings.key_id.as_deref(), Some("WRONGKEY99"));
        assert!(fx.ui.text().contains("--no-verify"));
    }

    #[tokio::test]
    async fn access_flags_change_the_profile_not_the_registration() {
        let cases = [(false, false), (true, false), (false, true), (true, true)];
        for (write, destructive) in cases {
            let mut fx = Fixture::new().with_stored_key(DEFAULT_PROFILE);
            let mut args = args(ClientKind::Codex);
            args.allow_write = write;
            args.allow_destructive = destructive;
            args.yes = true;
            fx.connect(&args).await.unwrap();

            let settings = fx.settings(DEFAULT_PROFILE);
            assert_eq!(settings.allow_write, write || destructive);
            assert_eq!(settings.allow_destructive, destructive);

            let config = fx.codex_config().unwrap();
            assert!(
                config.contains(&toml_args(&[
                    "serve",
                    "--profile",
                    "default",
                    "--dynamic-toolsets"
                ])),
                "{config}"
            );
        }

        // Без флагов права профиля не меняются.
        let mut fx = Fixture::new().with_stored_key(DEFAULT_PROFILE);
        let mut settings = fx.settings(DEFAULT_PROFILE);
        settings.allow_write = true;
        fx.repo.upsert(DEFAULT_PROFILE, settings).unwrap();
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert!(fx.settings(DEFAULT_PROFILE).allow_write);
    }

    #[tokio::test]
    async fn raising_access_needs_approval_on_macos() {
        let mut fx = Fixture::new().with_stored_key(DEFAULT_PROFILE);
        fx.approval = Some((MemoryApprovals::default(), FakePresence::new(false)));
        let mut args = args(ClientKind::Codex);
        args.allow_destructive = true;
        args.yes = true;
        fx.connect(&args).await.unwrap();
        // Отказ: права прежние, но профиль и регистрация сохранены.
        let settings = fx.settings(DEFAULT_PROFILE);
        assert!(!settings.allow_write && !settings.allow_destructive);
        let text = fx.ui.text();
        assert!(text.contains("The access was not changed"), "{text}");
        assert!(
            text.contains("`appstore-connect-mcp ui` (profile `default`)"),
            "{text}"
        );
        assert!(fx.codex_config().is_some());

        fx.approval = Some((MemoryApprovals::default(), FakePresence::new(true)));
        fx.ui = ScriptedUi::default();
        fx.connect(&args).await.unwrap();
        let settings = fx.settings(DEFAULT_PROFILE);
        assert!(settings.allow_destructive);
        let (approvals, presence) = fx.approval.as_ref().unwrap();
        assert_eq!(presence.asked.get(), 1);
        assert_eq!(
            approvals.load(DEFAULT_PROFILE).unwrap().unwrap().level,
            AccessLevel::Destructive
        );
        assert!(
            fx.ui.text().contains("approved on this Mac"),
            "{}",
            fx.ui.text()
        );
    }

    #[tokio::test]
    async fn ssh_session_only_prints_the_link() {
        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Skip(SkipReason::Ssh);
        let (key_file, _) = fx.key_file("AuthKey_TESTKEY123.p8");
        fx.answers(&[&key_file.display().to_string(), "", "n"]);
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert!(fx.ui.opened.is_empty());
        let text = fx.ui.text();
        assert!(text.contains(API_KEYS_URL), "{text}");
        assert!(text.contains("SSH session"), "{text}");
        // Пустой Issuer ID — индивидуальный ключ.
        assert!(fx.settings(DEFAULT_PROFILE).individual);
    }

    #[tokio::test]
    async fn browser_failure_only_prints_the_link() {
        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Open;
        let (key_file, _) = fx.key_file("AuthKey_TESTKEY123.p8");
        fx.answers(&[&key_file.display().to_string(), ISSUER, "n"]);
        fx.ui.open_fails = true;
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        let text = fx.ui.text();
        assert!(text.contains("Could not open a browser"), "{text}");
        assert!(fx.store.contains(DEFAULT_PROFILE).unwrap());
    }

    #[tokio::test]
    async fn key_file_and_issuer_flags_skip_the_guide() {
        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Open;
        let (key_file, _) = fx.key_file("AuthKey_TESTKEY123.p8");
        fx.answers(&["y"]);
        let mut args = with_key_file(args(ClientKind::Codex), &key_file);
        args.issuer_id = Some(ISSUER.into());
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert!(fx.ui.opened.is_empty());
        let text = fx.ui.text();
        assert!(!text.contains(API_KEYS_URL), "{text}");
        assert!(!text.contains("Step 1 of 4"), "{text}");
        assert!(!text.contains("vendor number"), "{text}");
        // Удалить файл — да: он удаляется после регистрации.
        assert!(!key_file.exists());
        assert!(text.contains("Deleted"), "{text}");
    }

    #[tokio::test]
    async fn no_input_needs_yes_and_never_asks() {
        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Open;
        let (key_file, _) = fx.key_file("AuthKey_TESTKEY123.p8");
        let mut quiet = with_key_file(args(ClientKind::Codex), &key_file);
        quiet.no_input = true;
        let err = fx.connect(&quiet).await.unwrap_err().to_string();
        assert!(err.contains("add --yes"), "{err}");

        quiet.yes = true;
        let err = fx.connect(&quiet).await.unwrap_err().to_string();
        assert!(
            err.contains("--issuer-id <UUID> (or --individual)"),
            "{err}"
        );
        assert!(!fx.store.contains(DEFAULT_PROFILE).unwrap());

        quiet.issuer_id = Some(ISSUER.into());
        fx.answers(&[]);
        fx.connect(&quiet).await.unwrap();
        assert!(fx.ui.questions.is_empty(), "{:?}", fx.ui.questions);
        assert!(fx.ui.opened.is_empty());
        assert!(fx.codex_config().is_some());
        assert!(key_file.exists());
    }

    #[tokio::test]
    async fn missing_issuer_id_shows_where_to_find_it() {
        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Open;
        let (key_file, _) = fx.key_file("AuthKey_TESTKEY123.p8");
        // Неверный Issuer ID переспрашивается.
        fx.answers(&["not-a-uuid", ISSUER, "n"]);
        let mut args = with_key_file(args(ClientKind::Codex), &key_file);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert_eq!(fx.ui.opened, [API_KEYS_URL]);
        let text = fx.ui.text();
        assert!(text.contains("above the list of team keys"), "{text}");
        assert!(text.contains("is not an Issuer ID"), "{text}");
        assert!(!text.contains("Step 1 of 4"), "{text}");
        assert_eq!(
            fx.settings(DEFAULT_PROFILE).issuer_id.as_deref(),
            Some(ISSUER)
        );
    }

    #[tokio::test]
    async fn declined_config_write_keeps_key_and_profile() {
        let mut fx = Fixture::new().with_stored_key(DEFAULT_PROFILE);
        fx.answers(&["n"]);
        fx.connect(&args(ClientKind::Codex)).await.unwrap();
        assert!(fx.codex_config().is_none());
        assert!(fx.store.contains(DEFAULT_PROFILE).unwrap());
        assert_eq!(
            fx.settings(DEFAULT_PROFILE).client.as_deref(),
            Some("codex")
        );
        assert!(fx.ui.text().contains("config was not changed"));
    }

    #[tokio::test]
    async fn existing_codex_config_is_preserved() {
        let mut fx = Fixture::new().with_stored_key(DEFAULT_PROFILE);
        fs::create_dir_all(&fx.codex_home).unwrap();
        let existing =
            "# my settings\nmodel = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"npx\"  # keep\n";
        fs::write(codex::config_path(&fx.codex_home), existing).unwrap();
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        let config = fx.codex_config().unwrap();
        assert!(config.starts_with(existing), "{config}");
        assert!(
            config.contains("[mcp_servers.appstore-connect]"),
            "{config}"
        );
    }

    #[tokio::test]
    async fn claude_flow_with_custom_name_and_project_scope() {
        let mut fx = Fixture::new().with_stored_key("work");
        fx.answers(&["y"]);
        let mut args = args(ClientKind::ClaudeCode);
        args.profile.profile = "work".into();
        args.scope = Scope::Project;
        args.name = Some("asc-team".into());
        fx.connect(&args).await.unwrap();

        assert_eq!(
            fx.runner.calls,
            [
                vec!["--version".to_owned()],
                claude::add_args(
                    Scope::Project,
                    &ServerSpec {
                        name: "asc-team".into(),
                        profile: "work".into(),
                        command: TEST_EXE.into(),
                        args: vec![
                            "serve".into(),
                            "--profile".into(),
                            "work".into(),
                            "--dynamic-toolsets".into()
                        ],
                    }
                )
            ]
        );
        assert_eq!(fx.settings("work").client.as_deref(), Some("claude-code"));
        assert!(fx.codex_config().is_none());
    }

    #[tokio::test]
    async fn long_server_name_is_warned_about() {
        let mut fx = Fixture::new().with_stored_key("production-account");
        let mut args = args(ClientKind::ClaudeCode);
        args.profile.profile = "production-account".into();
        args.yes = true;
        fx.connect(&args).await.unwrap();
        let text = fx.ui.text();
        assert!(
            text.contains("server name `asc-production-account` is longer than 17"),
            "{text}"
        );
        assert!(fx.runner.calls.len() == 2, "{:?}", fx.runner.calls);
    }

    #[tokio::test]
    async fn claude_missing_cli_fails_before_questions() {
        let mut fx = Fixture::new();
        fx.runner.results = [claude::CommandOutput {
            success: false,
            stderr: "broken".into(),
            ..Default::default()
        }]
        .into();
        let err = fx.connect(&args(ClientKind::ClaudeCode)).await.unwrap_err();
        assert!(err.to_string().contains("broken"), "{err}");
        assert!(fx.ui.output.is_empty());
        assert!(fx.ui.questions.is_empty());
    }

    #[tokio::test]
    async fn invalid_input_fails_before_questions() {
        let mut cases = Vec::new();
        let mut codex_project = args(ClientKind::Codex);
        codex_project.scope = Scope::Project;
        cases.push((codex_project, "--scope user"));
        let mut toolset = args(ClientKind::Codex);
        toolset.toolsets = vec!["Bad Set".into()];
        cases.push((toolset, "invalid toolset"));
        let mut profile = args(ClientKind::Codex);
        profile.profile.profile = "--evil".into();
        cases.push((profile, "invalid profile name"));
        let mut name = args(ClientKind::Codex);
        name.name = Some("a b".into());
        cases.push((name, "invalid server name"));
        let mut long_profile = args(ClientKind::Codex);
        long_profile.profile.profile = "x".repeat(64);
        cases.push((long_profile, "--name"));
        let mut key_id = args(ClientKind::Codex);
        key_id.key_id = Some("bad id".into());
        cases.push((key_id, "invalid Key ID"));
        let mut issuer = args(ClientKind::Codex);
        issuer.issuer_id = Some("nope".into());
        cases.push((issuer, "invalid Issuer ID"));
        let mut vendor = args(ClientKind::Codex);
        vendor.vendor_number = Some("12ab".into());
        cases.push((vendor, "invalid vendor number"));

        for (args, expected) in cases {
            let mut fx = Fixture::new();
            fx.answers(&[]);
            let err = fx.connect(&args).await.unwrap_err();
            assert!(format!("{err:#}").contains(expected), "{expected}: {err:#}");
            assert!(fx.ui.output.is_empty());
            assert!(fx.ui.questions.is_empty());
            assert_eq!(fx.repo.get(DEFAULT_PROFILE).unwrap(), None);
        }
    }
}
