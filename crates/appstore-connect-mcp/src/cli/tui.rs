//! `appstore-connect-mcp ui` — TUI профилей ([`mcp_kit::tui`]): профили и их настройки,
//! права и одобрение на macOS, регистрации в Claude Code и Codex, проверка ключа в App
//! Store Connect и инструменты с вызовом.
//!
//! Здесь только сервисная часть ([`AscAdapter`]): чтение и запись `profiles.toml`, состояние
//! ключа (без самого ключа), проверка ключа запросами к API, сборка сервера и повторная
//! регистрация без флагов доступа. Сервер для инструментов собирается так же, как `serve`
//! без флагов: права профиля, урезанные до одобренных.

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use appstore_connect_api::auth::{KeyStore, KeyringStore};
use clap::Args;
use mcp_kit::ProfileAccess;
use mcp_kit::approval;
use mcp_kit::call::Session;
use mcp_kit::registrations::{self, Registration, ServerIdentity};
use mcp_kit::tui::{self, Field, ProfileView, ServiceAdapter};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use serde_json::{Value, json};

use crate::cli::serve::{DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_TIMEOUT_SECS, ServeArgs, Setup};
use crate::clients::claude::{self, SystemRunner};
use crate::clients::{EXE_STEM, Scope, ServerSpec, codex};
use crate::profiles::{DEFAULT_PROFILE, PROFILE_ENV, ProfileRepo, ProfileSettings};
use crate::ui::CaptureUi;

const UI_HELP: &str = "\
Shows the profiles in profiles.toml with their settings and the state of the API key (never \
the key), the access of each profile and whether it is approved, where the server is \
registered in Claude Code and Codex and whether those registrations agree with the profile, \
what the key can see in App Store Connect, and the tools the profile gives the agent.

The access (read, write or destructive, and the extra directories for local files) is changed \
here: r, w and d set the level, a adds a directory, x removes one, s saves. On macOS, saving \
more access than was approved asks for Touch ID or the macOS password; access that is not \
approved does not take effect. f registers the selected entry again without access flags. \
Press ? for the keys.";

#[derive(Debug, Clone, Args)]
#[command(after_help = UI_HELP)]
pub struct UiArgs {}

pub async fn run(_args: UiArgs) -> anyhow::Result<()> {
    let adapter = AscAdapter {
        repo: ProfileRepo::open_default()?,
        keys: &KeyringStore,
        exe: std::env::current_exe()
            .map_err(|err| anyhow!("cannot determine the path of appstore-connect-mcp: {err}"))?,
        codex_home: codex::default_codex_home().ok(),
    };
    tui::run(&adapter).await.map_err(|err| anyhow!(err))
}

/// Сервисная часть TUI для App Store Connect.
pub struct AscAdapter<'a> {
    pub repo: ProfileRepo,
    pub keys: &'a dyn KeyStore,
    /// Путь к бинарнику для повторной регистрации.
    pub exe: PathBuf,
    /// `$CODEX_HOME`; `None` — не определить (тогда Codex не регистрируется заново).
    pub codex_home: Option<PathBuf>,
}

impl AscAdapter<'_> {
    /// Сервер с правами профиля без флагов (как `serve` без флагов), со всеми наборами.
    fn setup(&self, profile: &str) -> anyhow::Result<Setup> {
        let args = ServeArgs {
            profile: Some(profile.to_owned()),
            allow_write: false,
            allow_destructive: false,
            toolsets: vec![appstore_connect_mcp_core::ALL_TOOLSETS.to_owned()],
            dynamic_toolsets: false,
            tools_file: Vec::new(),
            allow_path: Vec::new(),
            timeout: DEFAULT_TIMEOUT_SECS,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            log_level: None,
            status: false,
        };
        let approvals = approval::platform_store(EXE_STEM);
        let mut setup = Setup::load(&args, self.keys, &self.repo, approvals.as_deref())?;
        setup.options.dynamic_toolsets = false;
        Ok(setup)
    }

    async fn session(&self, profile: &str) -> Result<Session, String> {
        let setup = self.setup(profile).map_err(|err| format!("{err:#}"))?;
        let server = setup.server().map_err(|err| format!("{err:#}"))?;
        Session::start(server).await.map_err(|err| err.to_string())
    }

    fn profile_fields(&self, name: &str, settings: &ProfileSettings) -> Vec<Field> {
        let key = match self.keys.contains(name) {
            Ok(true) => "stored in the system keychain".to_owned(),
            Ok(false) => format!("missing: run `appstore-connect-mcp auth login --profile {name}`"),
            Err(err) => format!("cannot check the keychain: {err}"),
        };
        let issuer = match (&settings.issuer_id, settings.individual) {
            (_, true) => "individual key".to_owned(),
            (Some(id), false) => format!("team key, Issuer ID {id}"),
            (None, false) => "not set".to_owned(),
        };
        let toolsets = if settings.toolsets.is_empty() {
            "default".to_owned()
        } else {
            settings.toolsets.join(",")
        };
        vec![
            Field::new("API key", key),
            Field::new(
                "Key ID",
                settings.key_id.clone().unwrap_or_else(|| "not set".into()),
            ),
            Field::new("Key type", issuer),
            Field::new(
                "Vendor number",
                settings
                    .vendor_number
                    .clone()
                    .unwrap_or_else(|| "not set".into()),
            ),
            Field::new("Toolsets", toolsets),
            Field::new(
                "Dynamic toolsets",
                if settings.dynamic_toolsets {
                    "on"
                } else {
                    "off"
                },
            ),
            Field::new(
                "Connected to",
                settings.client.clone().unwrap_or_else(|| "-".into()),
            ),
        ]
    }
}

impl ServiceAdapter for AscAdapter<'_> {
    fn bin(&self) -> &str {
        EXE_STEM
    }

    fn service_name(&self) -> &str {
        "App Store Connect"
    }

    fn identity(&self) -> ServerIdentity<'_> {
        ServerIdentity {
            exe_stem: EXE_STEM,
            default_profile: DEFAULT_PROFILE,
            profile_env: PROFILE_ENV,
        }
    }

    fn load_profiles(&self) -> Result<Vec<ProfileView>, String> {
        let profiles = self.repo.list().map_err(|err| format!("{err:#}"))?;
        Ok(profiles
            .iter()
            .map(|(name, settings)| ProfileView {
                name: name.clone(),
                access: settings.access(),
                fields: self.profile_fields(name, settings),
            })
            .collect())
    }

    fn save_access(&self, profile: &str, access: &ProfileAccess) -> Result<(), String> {
        let mut settings = self
            .repo
            .get(profile)
            .map_err(|err| format!("{err:#}"))?
            .unwrap_or_default();
        settings.set_access(access);
        self.repo
            .upsert(profile, settings)
            .map_err(|err| format!("{err:#}"))
    }

    async fn check_account(&self, profile: &str) -> Result<Vec<Field>, String> {
        let session = self.session(profile).await?;
        let mut fields = Vec::new();
        for (label, path, hint) in ACCOUNT_PROBES {
            let arguments = object(json!({
                "path": path,
                "jq": ".meta.paging.total // (.data | length)",
            }));
            let value = match session.call("api_request", arguments).await {
                Ok(result) => probe_text(&result, hint),
                Err(err) => format!("failed: {err}"),
            };
            fields.push(Field::new(*label, value));
        }
        session.close().await;
        fields.push(Field::new(
            "Key role",
            "App Store Connect does not report it to the key; see Users and Access > \
             Integrations",
        ));
        Ok(fields)
    }

    async fn tools(&self, profile: &str) -> Result<Vec<Tool>, String> {
        let session = self.session(profile).await?;
        let tools = session.tools().await.map_err(|err| err.to_string());
        session.close().await;
        tools
    }

    async fn call_tool(
        &self,
        profile: &str,
        tool: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, String> {
        let session = self.session(profile).await?;
        let result = session
            .call(tool, arguments)
            .await
            .map_err(|err| err.to_string());
        session.close().await;
        result
    }

    fn reregister(&self, registration: &Registration) -> Result<String, String> {
        reregister(
            registration,
            &self.exe,
            self.codex_home.as_deref(),
            std::env::current_dir().ok().as_deref(),
        )
    }
}

/// Проверки ключа: что видно ключу в кабинете. Роль ключа App Store Connect не сообщает,
/// поэтому она видна только по тому, какие запросы проходят.
const ACCOUNT_PROBES: &[(&str, &str, &str)] = &[
    ("Apps", "/v1/apps?limit=1", "any role"),
    ("Users", "/v1/users?limit=1", "Admin role"),
    (
        "Certificates",
        "/v1/certificates?limit=1",
        "Admin or Developer role",
    ),
];

/// Ответ проверки: число записей или «нет доступа» с первой строкой ошибки.
fn probe_text(result: &CallToolResult, needs: &str) -> String {
    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    if result.is_error == Some(true) {
        let first = text.lines().next().unwrap_or("error");
        let first: String = first.chars().take(120).collect();
        return format!("no access (needs {needs}): {first}");
    }
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let count = match &value {
        Value::Object(map) => map.get("result").cloned().unwrap_or(Value::Null),
        other => other.clone(),
    };
    match count {
        Value::Number(n) => format!("{n} visible"),
        _ => "accessible".to_owned(),
    }
}

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    }
}

/// Регистрирует запись заново: та же команда `serve --profile <профиль>` с теми же наборами,
/// но без флагов доступа. Запись проекта (Claude Code) — только из каталога проекта.
fn reregister(
    registration: &Registration,
    exe: &Path,
    codex_home: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<String, String> {
    let serve = &registration.serve;
    let settings = ProfileSettings {
        toolsets: serve.toolsets.clone(),
        dynamic_toolsets: serve.dynamic_toolsets.unwrap_or(false),
        ..ProfileSettings::default()
    };
    let spec = ServerSpec::new(&registration.name, exe, &serve.profile, &settings)
        .map_err(|err| format!("{err:#}"))?;
    let mut ui = CaptureUi::default();
    let done = match registration.client {
        registrations::Client::ClaudeCode => {
            let scope = match &registration.scope {
                registrations::Scope::User => Scope::User,
                registrations::Scope::Project(dir) if Some(dir.as_path()) == cwd => Scope::Project,
                other => {
                    return Err(format!(
                        "`{}` is registered for {other}; run `appstore-connect-mcp connect \
                         claude-code --scope project --name {} --profile {}` in that project",
                        registration.name, registration.name, serve.profile
                    ));
                }
            };
            claude::register(&mut SystemRunner, &mut ui, scope, &spec, true)
        }
        registrations::Client::Codex => {
            let home = codex_home.ok_or("cannot determine $CODEX_HOME")?;
            codex::register(&mut ui, home, &spec, true)
        }
    }
    .map_err(|err| format!("{err:#}"))?;
    if !done {
        return Err(ui
            .lines
            .last()
            .cloned()
            .unwrap_or_else(|| "not changed".into()));
    }
    Ok(format!(
        "{} now runs `{}` as `{}`; restart the client to pick it up",
        registration.client.display_name(),
        spec.display(),
        spec.name
    ))
}

#[cfg(test)]
mod tests {
    use appstore_connect_api::PrivateKeyPem;
    use appstore_connect_api::auth::MemoryStore;
    use mcp_kit::registrations::ServeInvocation;

    use super::*;
    use crate::clients::TEST_EXE;
    use crate::testing::generated_key_pem;

    fn adapter<'a>(dir: &Path, keys: &'a MemoryStore) -> AscAdapter<'a> {
        AscAdapter {
            repo: ProfileRepo::in_dir(dir.join("config")),
            keys,
            exe: PathBuf::from(TEST_EXE),
            codex_home: Some(dir.join("codex")),
        }
    }

    #[test]
    fn shows_profiles_without_the_key_and_saves_access() {
        let dir = tempfile::tempdir().unwrap();
        let keys = MemoryStore::new();
        let pem = generated_key_pem();
        keys.set("work", &PrivateKeyPem::parse(&pem).unwrap())
            .unwrap();
        let adapter = adapter(dir.path(), &keys);
        adapter
            .repo
            .upsert(
                "work",
                ProfileSettings {
                    key_id: Some("TESTKEY123".into()),
                    individual: true,
                    allow_write: true,
                    ..ProfileSettings::default()
                },
            )
            .unwrap();
        adapter
            .repo
            .upsert("empty", ProfileSettings::default())
            .unwrap();

        let profiles = adapter.load_profiles().unwrap();
        assert_eq!(profiles.len(), 2);
        let work = profiles.iter().find(|p| p.name == "work").unwrap();
        assert_eq!(work.access.level, mcp_kit::AccessLevel::Write);
        let text = format!("{:?}", work.fields);
        assert!(text.contains("stored in the system keychain"), "{text}");
        assert!(text.contains("individual key"), "{text}");
        assert!(!text.contains("PRIVATE KEY"), "{text}");
        let empty = profiles.iter().find(|p| p.name == "empty").unwrap();
        assert!(format!("{:?}", empty.fields).contains("auth login --profile empty"));

        let root = dir.path().join("files");
        adapter
            .save_access(
                "work",
                &ProfileAccess::new(mcp_kit::AccessLevel::Destructive, vec![root.clone()]),
            )
            .unwrap();
        let saved = adapter.repo.get("work").unwrap().unwrap();
        assert!(saved.allow_destructive);
        assert_eq!(saved.allow_path, vec![root]);
        // Остальные настройки профиля не тронуты.
        assert_eq!(saved.key_id.as_deref(), Some("TESTKEY123"));
    }

    #[test]
    fn probe_results_are_short() {
        let ok = CallToolResult::success(vec![ContentBlock::text(
            r#"{"result": 7, "pagination": {}}"#,
        )]);
        assert_eq!(probe_text(&ok, "x"), "7 visible");
        let plain = CallToolResult::success(vec![ContentBlock::text("3")]);
        assert_eq!(probe_text(&plain, "x"), "3 visible");
        let err = CallToolResult::error(vec![ContentBlock::text(
            "App Store Connect returned 403 Forbidden\nmore",
        )]);
        assert_eq!(
            probe_text(&err, "Admin role"),
            "no access (needs Admin role): App Store Connect returned 403 Forbidden"
        );
    }

    fn registration(client: registrations::Client, scope: registrations::Scope) -> Registration {
        Registration {
            client,
            scope,
            name: "asc-work".into(),
            command: PathBuf::from(TEST_EXE),
            args: vec![],
            env_names: vec![],
            enabled: true,
            serve: ServeInvocation {
                subcommand: Some("serve".into()),
                profile: "work".into(),
                toolsets: vec!["apps".into()],
                dynamic_toolsets: Some(true),
                ..ServeInvocation::default()
            },
        }
    }

    #[test]
    fn reregisters_codex_without_access_flags() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            codex::config_path(&home),
            format!(
                "[mcp_servers.asc-work]\ncommand = {TEST_EXE:?}\nargs = [\"serve\", \"--profile\", \"work\", \"--allow-write\", \"--allow-destructive\"]\n"
            ),
        )
        .unwrap();
        let reg = registration(registrations::Client::Codex, registrations::Scope::User);
        let message = reregister(&reg, Path::new(TEST_EXE), Some(&home), None).unwrap();
        assert!(message.contains("Codex now runs"), "{message}");
        let config = std::fs::read_to_string(codex::config_path(&home)).unwrap();
        assert!(!config.contains("--allow"), "{config}");
        assert!(
            config.contains(r#"args = ["serve", "--profile", "work", "--toolsets", "apps", "--dynamic-toolsets"]"#),
            "{config}"
        );
    }

    #[test]
    fn project_registration_elsewhere_is_explained() {
        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Project(PathBuf::from("/elsewhere")),
        );
        let err =
            reregister(&reg, Path::new(TEST_EXE), None, Some(Path::new("/here"))).unwrap_err();
        assert!(err.contains("--scope project"), "{err}");
        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Local(PathBuf::from("/here")),
        );
        assert!(reregister(&reg, Path::new(TEST_EXE), None, Some(Path::new("/here"))).is_err());
    }
}
