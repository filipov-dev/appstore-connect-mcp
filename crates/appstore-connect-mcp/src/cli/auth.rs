//! `appstore-connect-mcp auth` — ключи API и настройки профилей.
//!
//! Ключ `.p8` принимается только путём к файлу (`--key-file` или ответом на вопрос), не
//! содержимым в аргументе или переменной окружения. Он проверяется (PKCS#8, EC P-256),
//! по умолчанию проверяется запросом `GET /v1/apps?limit=1` и сохраняется только в
//! системном хранилище (общая с `connect` часть — в [`crate::login`]). Команды, которая
//! выводит ключ или JWT, нет и быть не должно.
//!
//! В терминале без `--key-file` `auth login` ведёт по шагам: что такое ключ, Key ID и
//! Issuer ID, как создать ключ (страница ключей открывается в браузере), где скачанный
//! файл, и предлагает сохранить vendor number. `--no-input` отключает вопросы, браузер и
//! пояснения: всё берётся из флагов и профиля.
//!
//! После сохранения `auth login` в терминале предлагает удалить исходный файл (по
//! умолчанию — нет: App Store Connect даёт скачать ключ только один раз, и файл может
//! быть единственной резервной копией). Без терминала (CI, скрипты) вопроса нет и файл
//! не трогается.

use std::path::PathBuf;

use anyhow::bail;
use appstore_connect_api::auth::{AuthError, KEYRING_SERVICE, KeyStore, KeyringStore};
use appstore_connect_api::{ApiKey, Issuer};
use clap::{Args, Subcommand};

pub use super::ProfileArg;
use crate::login::{
    Guide, KeyInput, offer_to_delete_key_file, offer_vendor_number, remember_key, store_new_key,
};
use crate::profiles::{
    DEFAULT_PROFILE, ProfileRepo, validate_profile_name, validate_vendor_number,
};
use crate::ui::{SilentUi, TerminalUi, Ui};

const LOGIN_HELP: &str = "\
In a terminal without --key-file, login walks you through getting a key: what the key file, \
the Key ID and the Issuer ID are, how to create a team key in App Store Connect (the API keys \
page opens in your browser), where to find the downloaded AuthKey_<KEYID>.p8 and the Issuer \
ID, and, optionally, the vendor number for sales and finance reports. Wrong answers are asked \
again.

With --no-input (alias --silent) nothing is asked and no browser is opened: pass --key-file, \
--issuer-id or --individual (unless saved in the profile) and --key-id (unless the file is \
named AuthKey_<KEYID>.p8). Without a terminal (CI, scripts) there is no guide either.

Links open in the default browser; over SSH, without a graphical session or with --no-browser \
they are only printed.";
use crate::verify::{ApiVerifier, CHECK_REQUEST, KeyVerifier};

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store an App Store Connect API key (.p8) and its Key ID and Issuer ID for a profile.
    ///
    /// In a terminal without --key-file, walks you through creating the key in App Store
    /// Connect step by step; --no-input runs without questions.
    ///
    /// The key is read from the file given with --key-file (or asked for), validated as an
    /// EC P-256 PKCS#8 key, checked with GET /v1/apps?limit=1 unless --no-verify, and saved
    /// in the system keychain. The key itself is never written to profiles.toml or printed.
    /// Replaces the stored key of the profile if there is one. Missing values are asked for
    /// in a terminal; the Key ID defaults to the one in the file name AuthKey_<KEYID>.p8.
    Login(LoginArgs),
    /// Show the profile, its Key ID, key type and whether the key is stored, and check the
    /// key with GET /v1/apps?limit=1.
    ///
    /// The key and the JWT are never printed. Exits with an error if the profile is
    /// incomplete, the key is missing or the check fails.
    Status(StatusArgs),
    /// Remove the stored API key of the profile from the system keychain.
    Logout(LogoutArgs),
}

#[derive(Debug, Clone, Args)]
#[command(after_help = LOGIN_HELP)]
pub struct LoginArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// Path to the API key file downloaded from App Store Connect (AuthKey_<KEYID>.p8).
    #[arg(long, value_name = "PATH")]
    pub key_file: Option<PathBuf>,
    /// Key ID of the key. Default: the KEYID part of the file name AuthKey_<KEYID>.p8.
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
    /// Store the key without checking it against the App Store Connect API.
    #[arg(long)]
    pub no_verify: bool,
    /// Do not ask anything and do not open a browser: take every value from flags and the
    /// profile, fail with the list of missing flags otherwise.
    #[arg(long, visible_alias = "silent")]
    pub no_input: bool,
    /// Do not open links in a browser; only print them.
    #[arg(long)]
    pub no_browser: bool,
}

impl LoginArgs {
    pub fn key_input(&self) -> KeyInput<'_> {
        KeyInput {
            key_file: self.key_file.as_deref(),
            key_id: self.key_id.as_deref(),
            issuer_id: self.issuer_id.as_deref(),
            individual: self.individual,
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct StatusArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// Only check the profile settings and the keychain; do not call the App Store Connect
    /// API.
    #[arg(long, visible_alias = "offline")]
    pub no_verify: bool,
}

#[derive(Debug, Clone, Args)]
pub struct LogoutArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// Also remove the profile settings from profiles.toml.
    #[arg(long)]
    pub forget: bool,
}

pub async fn run(command: AuthCommand) -> anyhow::Result<()> {
    let store = KeyringStore;
    let repo = ProfileRepo::open_default()?;
    let mut ui = TerminalUi;
    match command {
        AuthCommand::Login(args) => {
            let verifier = (!args.no_verify).then(ApiVerifier::new).transpose()?;
            let guide = Guide::detect(args.no_browser);
            login(&store, &repo, verifier.as_ref(), &mut ui, &guide, &args).await
        }
        AuthCommand::Status(args) => {
            let verifier = (!args.no_verify).then(ApiVerifier::new).transpose()?;
            status(
                &store,
                &repo,
                verifier.as_ref(),
                &mut ui,
                &args.profile.profile,
            )
            .await
        }
        AuthCommand::Logout(args) => {
            logout(&store, &repo, &mut ui, &args.profile.profile, args.forget)
        }
    }
}

pub async fn login<V: KeyVerifier>(
    store: &dyn KeyStore,
    repo: &ProfileRepo,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    guide: &Guide,
    args: &LoginArgs,
) -> anyhow::Result<()> {
    let profile = args.profile.profile.as_str();
    validate_profile_name(profile)?;
    let mut settings = repo.get(profile)?.unwrap_or_default();
    let input = args.key_input();
    input.validate()?;
    let vendor_number = args
        .vendor_number
        .as_deref()
        .map(validate_vendor_number)
        .transpose()?;

    // Тихий режим: без вопросов, браузера и пояснений; недостающие флаги — одной ошибкой.
    let mut silent;
    let no_guide = Guide::none();
    let (ui, guide) = if args.no_input {
        silent = SilentUi::new(ui);
        (&mut silent as &mut dyn Ui, &no_guide)
    } else {
        (ui, guide)
    };

    let new_key = store_new_key(store, verifier, ui, guide, profile, &input, &settings).await?;
    remember_key(&mut settings, &new_key.key);
    if vendor_number.is_some() {
        settings.vendor_number = vendor_number;
    } else if new_key.guided {
        offer_vendor_number(ui, guide, &mut settings);
    }
    repo.upsert(profile, settings)?;
    ui.say(&format!(
        "Profile `{profile}` saved to {}.",
        repo.path().display()
    ));
    if new_key.guided {
        show_next_steps(ui, profile);
    }

    offer_to_delete_key_file(ui, &new_key.file);
    Ok(())
}

/// Что делать после сохранения ключа: подключить сервер к MCP-клиенту.
fn show_next_steps(ui: &mut dyn Ui, profile: &str) {
    let flag = if profile == DEFAULT_PROFILE {
        String::new()
    } else {
        format!(" --profile {profile}")
    };
    ui.say("");
    ui.say("Next: connect the server to your MCP client:");
    ui.say(&format!("  appstore-connect-mcp connect claude-code{flag}"));
    ui.say(&format!("  appstore-connect-mcp connect codex{flag}"));
    ui.say(&format!(
        "Other clients run `appstore-connect-mcp serve --profile {profile}` as a stdio MCP \
         server; check the profile with `appstore-connect-mcp auth status{flag}`."
    ));
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub async fn status<V: KeyVerifier>(
    store: &dyn KeyStore,
    repo: &ProfileRepo,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    profile: &str,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    let saved = repo.get(profile)?;
    let settings = saved.clone().unwrap_or_default();
    // Что мешает работе профиля; при непустом списке команда завершается ошибкой.
    let mut problems: Vec<String> = Vec::new();

    ui.say(&format!("Profile:   {profile}"));
    let key_id = match settings.key_id() {
        Ok(Some(id)) => {
            ui.say(&format!("Key ID:    {}", id.as_str()));
            Some(id)
        }
        Ok(None) => {
            ui.say("Key ID:    not set");
            problems.push("no Key ID".into());
            None
        }
        Err(err) => {
            ui.say(&format!("Key ID:    invalid ({err:#})"));
            problems.push("invalid Key ID".into());
            None
        }
    };
    let issuer = match settings.issuer() {
        Ok(Some(issuer)) => {
            match &issuer {
                Issuer::Team(id) => ui.say(&format!("Key type:  team (Issuer ID {})", id.as_str())),
                Issuer::Individual => ui.say("Key type:  individual"),
            }
            Some(issuer)
        }
        Ok(None) => {
            ui.say("Key type:  not set (no Issuer ID, not an individual key)");
            problems.push("no Issuer ID".into());
            None
        }
        Err(err) => {
            ui.say(&format!("Key type:  invalid ({err:#})"));
            problems.push("invalid Issuer ID".into());
            None
        }
    };
    let pem = match store.get(profile) {
        Ok(pem) => {
            ui.say(&format!(
                "Key:       stored in the system keychain (service `{KEYRING_SERVICE}`, account \
                 `{profile}`)"
            ));
            Some(pem)
        }
        Err(AuthError::NotFound { .. }) => {
            ui.say("Key:       not found in the system keychain");
            problems.push("no API key in the keychain".into());
            None
        }
        Err(err @ AuthError::Corrupted { .. }) => {
            ui.say("Key:       the stored value is not a valid API key");
            problems.push(err.to_string());
            None
        }
        Err(err) => {
            ui.say(&format!("Key:       unavailable ({err})"));
            problems.push("the system keychain is not available".into());
            None
        }
    };

    let check = match (key_id, issuer, pem) {
        (Some(key_id), Some(issuer), Some(pem)) => match ApiKey::new(&pem, key_id, issuer) {
            Err(err) => {
                problems.push(err.to_string());
                "-".to_owned()
            }
            Ok(key) => match verifier {
                None => "not checked (--no-verify)".to_owned(),
                Some(verifier) => match verifier.check(&key).await {
                    Ok(outcome) => format!("ok, {outcome}"),
                    Err(err) => {
                        problems.push(format!("{CHECK_REQUEST} failed"));
                        format!("failed: {err:#}")
                    }
                },
            },
        },
        _ => "skipped: the profile is incomplete".to_owned(),
    };
    ui.say(&format!("Check:     {check}"));
    ui.say(&format!(
        "Vendor:    {}",
        settings.vendor_number.as_deref().unwrap_or("-")
    ));
    if saved.is_some() {
        // Действующий уровень, как у `serve`: удаление подразумевает запись.
        ui.say(&format!("Write:     {}", yes_no(settings.writes_allowed())));
        ui.say(&format!(
            "Delete:    {}",
            yes_no(settings.allow_destructive)
        ));
        let toolsets = if settings.toolsets.is_empty() {
            "default".to_owned()
        } else {
            settings.toolsets.join(",")
        };
        ui.say(&format!("Toolsets:  {toolsets}"));
    } else {
        ui.say("Settings:  none (read-only defaults)");
    }
    ui.say(&format!("Profiles:  {}", repo.path().display()));

    if !problems.is_empty() {
        bail!(
            "profile `{profile}` is not ready: {}. Fix it with `appstore-connect-mcp auth login \
             --profile {profile}`",
            problems.join("; ")
        );
    }
    Ok(())
}

pub fn logout(
    store: &dyn KeyStore,
    repo: &ProfileRepo,
    ui: &mut dyn Ui,
    profile: &str,
    forget: bool,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    match store.delete(profile) {
        Ok(()) => ui.say(&format!(
            "API key of profile `{profile}` removed from the system keychain. If the key is no \
             longer needed anywhere, also revoke it in App Store Connect (Users and Access > \
             Integrations)."
        )),
        Err(AuthError::NotFound { .. }) => {
            ui.say(&format!("No API key stored for profile `{profile}`."));
        }
        Err(err) => return Err(err.into()),
    }
    if forget {
        if repo.remove(profile)? {
            ui.say(&format!(
                "Profile `{profile}` removed from {}.",
                repo.path().display()
            ));
        } else {
            ui.say(&format!("Profile `{profile}` has no saved settings."));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use appstore_connect_api::PrivateKeyPem;
    use appstore_connect_api::auth::MemoryStore;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::profiles::DEFAULT_PROFILE;
    use crate::testing::generated_key_pem;
    use crate::ui::testing::ScriptedUi;
    use crate::verify::CHECK_PATH;
    use crate::verify::testing::FakeVerifier;

    const ISSUER: &str = "01234567-89ab-cdef-0123-456789abcdef";

    fn verifier() -> FakeVerifier {
        FakeVerifier {
            valid_key_id: "TESTKEY123".into(),
        }
    }

    fn args(profile: &str) -> LoginArgs {
        LoginArgs {
            profile: ProfileArg {
                profile: profile.into(),
            },
            key_file: None,
            key_id: None,
            issuer_id: None,
            individual: false,
            vendor_number: None,
            no_verify: false,
            no_input: false,
            no_browser: false,
        }
    }

    struct Setup {
        dir: tempfile::TempDir,
        repo: ProfileRepo,
        store: MemoryStore,
    }

    impl Setup {
        /// Файл ключа `name` со сгенерированным ключом; возвращает путь и PEM.
        fn key_file(&self, name: &str) -> (PathBuf, String) {
            let pem = generated_key_pem();
            let path = self.dir.path().join(name);
            fs::write(&path, &pem).unwrap();
            (path, pem)
        }
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("config"));
        Setup {
            dir,
            repo,
            store: MemoryStore::new(),
        }
    }

    #[tokio::test]
    async fn login_status_logout_with_flags() {
        let s = setup();
        let mut ui = ScriptedUi::default();
        let err = status(&s.store, &s.repo, Some(&verifier()), &mut ui, "ci")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not ready"), "{err}");
        assert!(ui.text().contains("Key:       not found"), "{}", ui.text());

        let (key_file, pem) = s.key_file("test.p8");
        let login_args = LoginArgs {
            key_file: Some(key_file.clone()),
            key_id: Some("TESTKEY123".into()),
            issuer_id: Some(ISSUER.into()),
            vendor_number: Some("80000001".into()),
            ..args("ci")
        };
        let mut ui = ScriptedUi::default();
        login(
            &s.store,
            &s.repo,
            Some(&verifier()),
            &mut ui,
            &Guide::none(),
            &login_args,
        )
        .await
        .unwrap();
        assert!(ui.questions.is_empty());
        let text = ui.text();
        assert!(
            text.contains("Key is valid: GET /v1/apps?limit=1 returned HTTP 200"),
            "{text}"
        );
        assert!(text.contains("API key saved"), "{text}");
        assert!(!text.contains(pem.lines().nth(1).unwrap()), "{text}");
        assert_eq!(s.store.get("ci").unwrap().expose(), pem);
        // Без терминала файл ключа не трогается и вопроса нет.
        assert!(key_file.exists());

        let saved = s.repo.get("ci").unwrap().unwrap();
        assert_eq!(saved.key_id.as_deref(), Some("TESTKEY123"));
        assert_eq!(saved.issuer_id.as_deref(), Some(ISSUER));
        assert_eq!(saved.vendor_number.as_deref(), Some("80000001"));
        assert!(!saved.allow_write);
        // Ключ в файл профилей не пишется.
        let file = fs::read_to_string(s.repo.path()).unwrap();
        assert!(!file.contains("PRIVATE KEY"), "{file}");

        let mut ui = ScriptedUi::default();
        status(&s.store, &s.repo, Some(&verifier()), &mut ui, "ci")
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("Key ID:    TESTKEY123"), "{text}");
        assert!(
            text.contains(&format!("Key type:  team (Issuer ID {ISSUER})")),
            "{text}"
        );
        assert!(text.contains("Key:       stored"), "{text}");
        assert!(
            text.contains("Check:     ok, GET /v1/apps?limit=1 returned HTTP 200"),
            "{text}"
        );
        assert!(text.contains("Write:     no"), "{text}");
        assert!(!text.contains("PRIVATE KEY"), "{text}");

        // --no-verify: только хранилище, без запроса.
        let mut ui = ScriptedUi::default();
        status::<FakeVerifier>(&s.store, &s.repo, None, &mut ui, "ci")
            .await
            .unwrap();
        assert!(
            ui.text().contains("not checked (--no-verify)"),
            "{}",
            ui.text()
        );

        let mut ui = ScriptedUi::default();
        logout(&s.store, &s.repo, &mut ui, "ci", false).unwrap();
        assert!(!s.store.contains("ci").unwrap());
        assert!(s.repo.get("ci").unwrap().is_some());
        let err = status::<FakeVerifier>(&s.store, &s.repo, None, &mut ui, "ci")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no API key in the keychain"),
            "{err}"
        );

        // Повторный logout — не ошибка; --forget удаляет настройки.
        let mut ui = ScriptedUi::default();
        logout(&s.store, &s.repo, &mut ui, "ci", true).unwrap();
        assert!(ui.text().contains("No API key stored"));
        assert_eq!(s.repo.get("ci").unwrap(), None);
    }

    #[tokio::test]
    async fn login_asks_for_missing_values_and_offers_to_delete_the_file() {
        let s = setup();
        let (key_file, _) = s.key_file("AuthKey_TESTKEY123.p8");
        // Путь в кавычках, как при перетаскивании файла в терминал; Issuer ID; vendor number
        // — не сейчас; удалить — да.
        let quoted = format!("'{}'", key_file.display());
        let mut ui = ScriptedUi::answering(&[&quoted, ISSUER, "n", "y"]);
        login(
            &s.store,
            &s.repo,
            Some(&verifier()),
            &mut ui,
            &Guide::none(),
            &args(DEFAULT_PROFILE),
        )
        .await
        .unwrap();
        assert_eq!(ui.questions.len(), 4, "{:?}", ui.questions);
        assert!(ui.questions[0].contains("Path to the key file"));
        assert!(ui.questions[1].contains("Issuer ID"));
        assert!(ui.questions[2].contains("vendor number"));
        assert!(ui.questions[3].contains("Delete the key file"));
        let text = ui.text();
        // Мастер объясняет, что нужно и где это взять.
        for expected in [
            "Step 1 of 4. What you need",
            "Step 2 of 4. Create a team key",
            "Step 3 of 4. The key file",
            "Step 4 of 4. Issuer ID",
            "Account Holder",
            "Admin",
            crate::login::API_KEYS_URL,
            "Key ID: TESTKEY123 (from the file name).",
            "Next: connect the server to your MCP client",
            "appstore-connect-mcp connect claude-code",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        assert!(!key_file.exists());
        let saved = s.repo.get(DEFAULT_PROFILE).unwrap().unwrap();
        assert_eq!(saved.key_id.as_deref(), Some("TESTKEY123"));
        assert_eq!(saved.issuer_id.as_deref(), Some(ISSUER));
        assert_eq!(saved.vendor_number, None);

        // Повторный вход: Issuer ID из профиля, пустой ответ на удаление — оставить файл.
        let (key_file, pem) = s.key_file("other.p8");
        let mut ui = ScriptedUi::answering(&["TESTKEY123", ""]);
        let rerun = LoginArgs {
            key_file: Some(key_file.clone()),
            ..args(DEFAULT_PROFILE)
        };
        login(
            &s.store,
            &s.repo,
            Some(&verifier()),
            &mut ui,
            &Guide::none(),
            &rerun,
        )
        .await
        .unwrap();
        let text = ui.text();
        assert!(text.contains("(from the profile)"), "{text}");
        assert!(text.contains("API key replaced"), "{text}");
        assert!(text.contains("Kept"), "{text}");
        assert!(key_file.exists());
        assert_eq!(s.store.get(DEFAULT_PROFILE).unwrap().expose(), pem);
    }

    #[tokio::test]
    async fn wizard_finds_the_downloaded_key_and_saves_the_vendor_number() {
        let s = setup();
        let downloads = s.dir.path().join("Downloads");
        fs::create_dir(&downloads).unwrap();
        let key_file = downloads.join("AuthKey_TESTKEY123.p8");
        let pem = generated_key_pem();
        fs::write(&key_file, &pem).unwrap();
        let guide = Guide {
            browser: crate::browser::BrowserDecision::Open,
            downloads: Some(downloads),
        };
        // Enter после скачивания; найденный файл — да; Issuer ID; vendor number — да,
        // сначала неверный, потом верный; удалить файл — нет.
        let mut ui = ScriptedUi::answering(&["", "", ISSUER, "y", "12ab", "80000001", "n"]);
        login(
            &s.store,
            &s.repo,
            Some(&verifier()),
            &mut ui,
            &guide,
            &args("work"),
        )
        .await
        .unwrap();
        assert_eq!(
            ui.opened,
            [crate::login::API_KEYS_URL, crate::login::PAYMENTS_URL]
        );
        assert!(
            ui.questions[1].contains("AuthKey_TESTKEY123.p8"),
            "{:?}",
            ui.questions
        );
        assert!(ui.questions[1].contains("just now"), "{:?}", ui.questions);
        let text = ui.text();
        assert!(text.contains("invalid vendor number"), "{text}");
        assert!(text.contains("--profile work"), "{text}");
        assert!(!text.contains(pem.lines().nth(1).unwrap()), "{text}");
        let saved = s.repo.get("work").unwrap().unwrap();
        assert_eq!(saved.vendor_number.as_deref(), Some("80000001"));
        assert_eq!(s.store.get("work").unwrap().expose(), pem);
        assert!(key_file.exists());
    }

    #[tokio::test]
    async fn wizard_asks_again_after_wrong_answers_and_a_failed_check() {
        let s = setup();
        // Имя файла даёт неверный Key ID: проверка не пройдёт с первого раза.
        let (key_file, _) = s.key_file("AuthKey_WRONGKEY99.p8");
        let missing = s.dir.path().join("missing.p8");
        let mut ui = ScriptedUi::answering(&[
            missing.to_str().unwrap(),
            key_file.to_str().unwrap(),
            "nope",
            ISSUER,
            // Проверка не прошла: ввести снова — да (пустой ответ), Key ID, Issuer ID.
            "",
            "TESTKEY123",
            ISSUER,
            // Vendor number — нет, удалить файл — нет.
            "n",
            "n",
        ]);
        login(
            &s.store,
            &s.repo,
            Some(&verifier()),
            &mut ui,
            &Guide::none(),
            &args(DEFAULT_PROFILE),
        )
        .await
        .unwrap();
        let text = ui.text();
        assert!(text.contains("cannot open the key file"), "{text}");
        assert!(text.contains("is not an Issuer ID"), "{text}");
        assert!(text.contains("The key check failed"), "{text}");
        assert!(text.contains("Key is valid"), "{text}");
        let saved = s.repo.get(DEFAULT_PROFILE).unwrap().unwrap();
        assert_eq!(saved.key_id.as_deref(), Some("TESTKEY123"));
    }

    #[tokio::test]
    async fn wizard_gives_up_after_three_wrong_answers() {
        let s = setup();
        let mut ui = ScriptedUi::answering(&["/nope/1.p8", "/nope/2.p8", "/nope/3.p8"]);
        let err = login(
            &s.store,
            &s.repo,
            Some(&verifier()),
            &mut ui,
            &Guide::none(),
            &args(DEFAULT_PROFILE),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot open the key file"),
            "{err:#}"
        );
        assert_eq!(ui.questions.len(), 3, "{:?}", ui.questions);
        assert!(!s.store.contains(DEFAULT_PROFILE).unwrap());
    }

    #[tokio::test]
    async fn no_input_never_asks_and_lists_missing_flags() {
        let s = setup();
        // Терминал есть, но --no-input: ни вопросов, ни браузера.
        let mut ui = ScriptedUi::answering(&[]);
        let quiet = LoginArgs {
            no_input: true,
            ..args("ci")
        };
        let guide = Guide {
            browser: crate::browser::BrowserDecision::Open,
            downloads: None,
        };
        let err = login(
            &s.store,
            &s.repo,
            Some(&verifier()),
            &mut ui,
            &guide,
            &quiet,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("--key-file"), "{err}");
        assert!(
            err.contains("--issuer-id <UUID> (or --individual)"),
            "{err}"
        );
        assert!(ui.questions.is_empty());
        assert!(ui.opened.is_empty());

        let (key_file, _) = s.key_file("AuthKey_TESTKEY123.p8");
        let complete = LoginArgs {
            key_file: Some(key_file.clone()),
            issuer_id: Some(ISSUER.into()),
            ..quiet
        };
        let mut ui = ScriptedUi::answering(&[]);
        login(
            &s.store,
            &s.repo,
            Some(&verifier()),
            &mut ui,
            &guide,
            &complete,
        )
        .await
        .unwrap();
        assert!(ui.questions.is_empty(), "{:?}", ui.questions);
        assert!(ui.opened.is_empty());
        let text = ui.text();
        assert!(!text.contains("Step 1"), "{text}");
        assert!(!text.contains("Next:"), "{text}");
        assert!(text.contains("API key saved"), "{text}");
        // Файл не трогается: вопроса об удалении нет.
        assert!(key_file.exists());
    }

    #[tokio::test]
    async fn empty_issuer_answer_means_individual_key() {
        let s = setup();
        let (key_file, _) = s.key_file("AuthKey_TESTKEY123.p8");
        let mut ui = ScriptedUi::answering(&["", "n"]);
        let login_args = LoginArgs {
            key_file: Some(key_file),
            ..args("solo")
        };
        login(
            &s.store,
            &s.repo,
            Some(&verifier()),
            &mut ui,
            &Guide::none(),
            &login_args,
        )
        .await
        .unwrap();
        let saved = s.repo.get("solo").unwrap().unwrap();
        assert!(saved.individual);
        assert_eq!(saved.issuer_id, None);

        let mut ui = ScriptedUi::default();
        status::<FakeVerifier>(&s.store, &s.repo, None, &mut ui, "solo")
            .await
            .unwrap();
        assert!(ui.text().contains("Key type:  individual"), "{}", ui.text());

        // --individual переводит командный профиль на индивидуальный ключ, и наоборот.
        let (key_file, _) = s.key_file("AuthKey_TESTKEY123.p8");
        let team = LoginArgs {
            key_file: Some(key_file),
            issuer_id: Some(ISSUER.into()),
            no_verify: true,
            ..args("solo")
        };
        login::<FakeVerifier>(
            &s.store,
            &s.repo,
            None,
            &mut ScriptedUi::default(),
            &Guide::none(),
            &team,
        )
        .await
        .unwrap();
        let saved = s.repo.get("solo").unwrap().unwrap();
        assert!(!saved.individual);
        assert_eq!(saved.issuer_id.as_deref(), Some(ISSUER));
    }

    #[tokio::test]
    async fn failures_store_nothing() {
        let s = setup();
        let (key_file, pem) = s.key_file("AuthKey_WRONGKEY99.p8");
        let secret_line = pem.lines().nth(1).unwrap().to_owned();
        let base = LoginArgs {
            key_file: Some(key_file.clone()),
            issuer_id: Some(ISSUER.into()),
            ..args("ci")
        };

        // Проверка не прошла.
        let err = login(
            &s.store,
            &s.repo,
            Some(&verifier()),
            &mut ScriptedUi::default(),
            &Guide::none(),
            &base,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("rejected the key"), "{err}");
        assert!(err.contains("Nothing was stored"), "{err}");
        assert!(err.contains("--no-verify"), "{err}");

        // Не ключ, неверные Key ID и Issuer ID, файла нет, без терминала нечего спросить.
        let not_a_key = s.dir.path().join("AuthKey_TESTKEY123.p8");
        fs::write(
            &not_a_key,
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let cases: Vec<(LoginArgs, &str)> = vec![
            (
                LoginArgs {
                    key_file: Some(not_a_key),
                    ..base.clone()
                },
                "not a usable API key",
            ),
            (
                LoginArgs {
                    key_id: Some("bad id".into()),
                    ..base.clone()
                },
                "invalid Key ID",
            ),
            (
                LoginArgs {
                    issuer_id: Some("not-a-uuid".into()),
                    ..base.clone()
                },
                "invalid Issuer ID",
            ),
            (
                LoginArgs {
                    vendor_number: Some("12ab".into()),
                    ..base.clone()
                },
                "invalid vendor number",
            ),
            (
                LoginArgs {
                    key_file: Some(s.dir.path().join("AuthKey_MISSING999.p8")),
                    ..base.clone()
                },
                "cannot open the key file",
            ),
            (args("ci"), "missing --key-file"),
            (
                LoginArgs {
                    key_file: Some(key_file.clone()),
                    issuer_id: None,
                    ..base.clone()
                },
                "--issuer-id <UUID> (or --individual)",
            ),
        ];
        for (case, expected) in cases {
            let err = login(
                &s.store,
                &s.repo,
                Some(&verifier()),
                &mut ScriptedUi::default(),
                &Guide::none(),
                &case,
            )
            .await
            .unwrap_err();
            let text = format!("{err:#}");
            assert!(text.contains(expected), "{expected}: {text}");
            assert!(!text.contains(&secret_line), "{text}");
        }
        assert!(!s.store.contains("ci").unwrap());
        assert_eq!(s.repo.get("ci").unwrap(), None);
    }

    #[tokio::test]
    async fn status_reports_failed_check_and_corrupted_settings() {
        let s = setup();
        fs::create_dir_all(s.repo.path().parent().unwrap()).unwrap();
        fs::write(
            s.repo.path(),
            format!(
                "[profiles.old]\nkey_id = \"OTHERKEY1\"\nissuer_id = \"{ISSUER}\"\nallow_destructive = true\n"
            ),
        )
        .unwrap();
        s.store
            .set("old", &PrivateKeyPem::parse(&generated_key_pem()).unwrap())
            .unwrap();
        let mut ui = ScriptedUi::default();
        let err = status(&s.store, &s.repo, Some(&verifier()), &mut ui, "old")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("GET /v1/apps?limit=1 failed"),
            "{err}"
        );
        let text = ui.text();
        assert!(text.contains("Check:     failed"), "{text}");
        assert!(text.contains("Write:     yes"), "{text}");
        assert!(text.contains("Delete:    yes"), "{text}");

        fs::write(s.repo.path(), "[profiles.old]\nissuer_id = \"x\"\n").unwrap();
        let mut ui = ScriptedUi::default();
        let err = status::<FakeVerifier>(&s.store, &s.repo, None, &mut ui, "old")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid Issuer ID"), "{err}");
        assert!(
            ui.text().contains("skipped: the profile is incomplete"),
            "{}",
            ui.text()
        );
    }

    /// Полный сценарий с настоящим исполнителем против локального wiremock.
    #[tokio::test]
    async fn login_and_status_against_mock_api() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(CHECK_PATH))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [],
                "meta": {"paging": {"total": 0, "limit": 1}}
            })))
            .expect(2)
            .mount(&server)
            .await;
        let api = ApiVerifier::with_base_url(&server.uri()).unwrap();
        let s = setup();
        let (key_file, _) = s.key_file("AuthKey_TESTKEY123.p8");
        let login_args = LoginArgs {
            key_file: Some(key_file),
            individual: true,
            ..args(DEFAULT_PROFILE)
        };
        let mut ui = ScriptedUi::default();
        login(
            &s.store,
            &s.repo,
            Some(&api),
            &mut ui,
            &Guide::none(),
            &login_args,
        )
        .await
        .unwrap();
        assert!(ui.text().contains("the key sees 0 apps"), "{}", ui.text());

        let mut ui = ScriptedUi::default();
        status(&s.store, &s.repo, Some(&api), &mut ui, DEFAULT_PROFILE)
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("Check:     ok"), "{text}");
        assert!(!text.contains("Bearer"), "{text}");
    }
}
