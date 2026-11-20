//! Профили: настройки в `~/.config/appstore-connect-mcp/profiles.toml`, ключ `.p8` — в
//! системном хранилище под именем профиля.
//!
//! Один профиль — один кабинет App Store Connect. В файле профилей нет ключей и других
//! секретов: только Key ID, Issuer ID (или признак индивидуального ключа), номер
//! поставщика для отчётов и настройки доступа по умолчанию.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use appstore_connect_api::auth::{AuthError, KeyStore, KeyringStore};
use appstore_connect_api::{ApiKey, Issuer, IssuerId, KeyId, PrivateKeyPem};
use mcp_kit::ProfileAccess;
use serde::{Deserialize, Serialize};

/// Переменная окружения, которая переопределяет каталог конфигурации (нужна для тестов).
pub const CONFIG_DIR_ENV: &str = "APPSTORE_CONNECT_MCP_CONFIG_DIR";

/// Переменная окружения с именем профиля по умолчанию (только имя, не ключ).
pub const PROFILE_ENV: &str = "APPSTORE_CONNECT_MCP_PROFILE";

/// Имя файла профилей в каталоге конфигурации.
pub const PROFILES_FILE: &str = "profiles.toml";

/// Профиль по умолчанию.
pub const DEFAULT_PROFILE: &str = "default";

/// Максимальная длина имени профиля.
const MAX_PROFILE_LEN: usize = 64;

/// Максимальная длина номера поставщика (Vendor Number). У Apple — 8–9 цифр.
const MAX_VENDOR_NUMBER_LEN: usize = 20;

/// Настройки профиля. Ключа здесь нет.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSettings {
    /// Key ID ключа API (`kid` JWT).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// Issuer ID командного ключа (`iss` JWT). Не задаётся вместе с `individual`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_id: Option<String>,
    /// Индивидуальный ключ: JWT с `sub = "user"` без `iss`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub individual: bool,
    /// Номер поставщика (Vendor Number) для отчётов продаж и финансов.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_number: Option<String>,
    /// MCP-клиент, в котором `connect` зарегистрировал профиль (`claude-code`, `codex`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Разрешены операции записи (`serve --allow-write`).
    pub allow_write: bool,
    /// Разрешены операции уровня удаления (`serve --allow-destructive`).
    pub allow_destructive: bool,
    /// Дополнительные каталоги для локальных файлов инструментов (`serve --allow-path`
    /// только сужает их). Абсолютные пути.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_path: Vec<PathBuf>,
    /// Включённые наборы инструментов (`serve --toolsets`); пустой список — набор по умолчанию.
    pub toolsets: Vec<String>,
    /// Наборы инструментов включаются агентом по запросу (`serve --dynamic-toolsets`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dynamic_toolsets: bool,
}

impl ProfileSettings {
    /// Действующее разрешение на запись: удаление подразумевает запись.
    pub fn writes_allowed(&self) -> bool {
        self.allow_write || self.allow_destructive
    }

    /// Приводит флаги доступа к действующему уровню: `allow_destructive` включает `allow_write`.
    pub fn normalized(mut self) -> Self {
        self.allow_write = self.writes_allowed();
        self
    }

    /// Права профиля: уровень доступа и каталоги. Флаги `serve` их только сужают, на macOS
    /// они ещё и сверяются с одобренными (см. [`mcp_kit::approval`]).
    pub fn access(&self) -> ProfileAccess {
        ProfileAccess::new(
            mcp_kit::AccessLevel::from_flags(self.allow_write, self.allow_destructive),
            self.allow_path.clone(),
        )
    }

    /// Записывает права в настройки.
    pub fn set_access(&mut self, access: &ProfileAccess) {
        (self.allow_write, self.allow_destructive) = access.level.to_flags();
        self.allow_path = access.allow_path.clone();
    }

    /// Key ID профиля, если задан. Некорректное значение в файле — ошибка.
    pub fn key_id(&self) -> anyhow::Result<Option<KeyId>> {
        self.key_id
            .as_deref()
            .map(KeyId::new)
            .transpose()
            .context("profiles.toml has an invalid `key_id`")
    }

    /// Тип ключа профиля, если задан. `issuer_id` вместе с `individual` — ошибка.
    pub fn issuer(&self) -> anyhow::Result<Option<Issuer>> {
        match (&self.issuer_id, self.individual) {
            (Some(_), true) => bail!(
                "profiles.toml sets both `issuer_id` and `individual = true`; a key is either a \
                 team key (Issuer ID) or an individual key"
            ),
            (Some(id), false) => Ok(Some(Issuer::Team(
                IssuerId::new(id).context("profiles.toml has an invalid `issuer_id`")?,
            ))),
            (None, true) => Ok(Some(Issuer::Individual)),
            (None, false) => Ok(None),
        }
    }

    /// Записывает тип ключа в настройки.
    pub fn set_issuer(&mut self, issuer: &Issuer) {
        match issuer {
            Issuer::Team(id) => {
                self.issuer_id = Some(id.as_str().to_owned());
                self.individual = false;
            }
            Issuer::Individual => {
                self.issuer_id = None;
                self.individual = true;
            }
        }
    }
}

/// Проверяет номер поставщика: только цифры.
pub fn validate_vendor_number(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_VENDOR_NUMBER_LEN
        || !value.bytes().all(|b| b.is_ascii_digit())
    {
        bail!(
            "invalid vendor number: expected the digits shown in App Store Connect \
             (Payments and Financial Reports)"
        );
    }
    Ok(value.to_owned())
}

/// Содержимое `profiles.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProfilesDocument {
    profiles: BTreeMap<String, ProfileSettings>,
}

/// Файл профилей.
#[derive(Debug, Clone)]
pub struct ProfileRepo {
    path: PathBuf,
}

impl ProfileRepo {
    /// Файл профилей в каталоге `dir`.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(PROFILES_FILE),
        }
    }

    /// Файл профилей в каталоге конфигурации пользователя.
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::in_dir(config_dir()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> anyhow::Result<ProfilesDocument> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProfilesDocument::default());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", self.path.display()));
            }
        };
        toml::from_str(&text).with_context(|| format!("failed to parse {}", self.path.display()))
    }

    fn save(&self, document: &ProfilesDocument) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(document).context("failed to serialize profiles")?;
        fs::write(&self.path, text)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    /// Настройки профиля, если он есть в файле.
    pub fn get(&self, name: &str) -> anyhow::Result<Option<ProfileSettings>> {
        Ok(self.load()?.profiles.remove(name))
    }

    /// Все профили файла по имени.
    pub fn list(&self) -> anyhow::Result<BTreeMap<String, ProfileSettings>> {
        Ok(self.load()?.profiles)
    }

    /// Создаёт или заменяет профиль. Флаги доступа сохраняются нормализованными
    /// (см. [`ProfileSettings::normalized`]).
    pub fn upsert(&self, name: &str, settings: ProfileSettings) -> anyhow::Result<()> {
        validate_profile_name(name)?;
        let mut document = self.load()?;
        document
            .profiles
            .insert(name.to_owned(), settings.normalized());
        self.save(&document)
    }

    /// Удаляет профиль. Возвращает `true`, если профиль был.
    pub fn remove(&self, name: &str) -> anyhow::Result<bool> {
        let mut document = self.load()?;
        let removed = document.profiles.remove(name).is_some();
        if removed {
            self.save(&document)?;
        }
        Ok(removed)
    }
}

/// Каталог конфигурации: `APPSTORE_CONNECT_MCP_CONFIG_DIR`, иначе
/// `~/.config/appstore-connect-mcp`.
///
/// На macOS тоже используется `~/.config`, а не `~/Library/Application Support`: так путь
/// одинаков на macOS и Linux. На Linux учитывается `XDG_CONFIG_HOME`.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let base = if cfg!(target_os = "macos") {
        dirs::home_dir().map(|home| home.join(".config"))
    } else {
        dirs::config_dir()
    };
    base.map(|base| base.join("appstore-connect-mcp"))
        .context("cannot determine the user configuration directory")
}

/// Проверяет имя профиля. Имя попадает в аргументы команд MCP-клиентов, ключи TOML и имя
/// записи в keychain, поэтому допускаются только `[A-Za-z0-9._-]` и имя не начинается с `-`.
pub fn validate_profile_name(name: &str) -> anyhow::Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if name.is_empty() || name.len() > MAX_PROFILE_LEN || !valid_chars || name.starts_with('-') {
        bail!(
            "invalid profile name `{name}`: use 1-{MAX_PROFILE_LEN} characters from A-Z, a-z, 0-9, `.`, `_`, `-`, not starting with `-`"
        );
    }
    Ok(())
}

/// Профиль, готовый к работе: настройки и ключ для выпуска JWT.
// Загрузку профиля целиком использует `serve`; `auth status` проверяет части профиля по
// отдельности, чтобы показать каждую проблему.
#[derive(Debug, Clone)]
pub struct Profile {
    pub settings: ProfileSettings,
    pub key: ApiKey,
}

/// Загружает профиль: настройки из `profiles.toml` и ключ из системного хранилища.
///
/// Отсутствие ключа, Key ID или Issuer ID — понятная пользователю ошибка с подсказкой.
#[allow(dead_code)]
pub fn load_profile(name: &str) -> anyhow::Result<Profile> {
    load_profile_with(&ProfileRepo::open_default()?, &KeyringStore, name)
}

/// То же, что [`load_profile`], с явными файлом профилей и хранилищем ключей.
pub fn load_profile_with(
    repo: &ProfileRepo,
    store: &dyn KeyStore,
    name: &str,
) -> anyhow::Result<Profile> {
    let stored = load_stored_key_with(repo, store, name)?;
    let key = ApiKey::new(&stored.pem, stored.key_id, stored.issuer)?;
    Ok(Profile {
        settings: stored.settings,
        key,
    })
}

/// Ключ профиля в виде, нужном `key exec`: PEM-текст для временного файла, Key ID и тип
/// ключа для переменных окружения. `Debug` PEM не раскрывает (см. [`PrivateKeyPem`]).
#[derive(Debug, Clone)]
pub struct StoredKey {
    pub settings: ProfileSettings,
    pub key_id: KeyId,
    pub issuer: Issuer,
    pub pem: PrivateKeyPem,
}

/// Загружает настройки профиля и его ключ из системного хранилища.
///
/// Отсутствие ключа, Key ID или Issuer ID — понятная пользователю ошибка с подсказкой.
pub fn load_stored_key_with(
    repo: &ProfileRepo,
    store: &dyn KeyStore,
    name: &str,
) -> anyhow::Result<StoredKey> {
    validate_profile_name(name)?;
    let settings = repo.get(name)?.unwrap_or_default();
    let login_hint = format!(
        "store the key with `appstore-connect-mcp auth login --profile {name} --key-file \
         <AuthKey_XXXXXXXXXX.p8>`"
    );
    let Some(key_id) = settings.key_id()? else {
        bail!("profile `{name}` has no Key ID; {login_hint}");
    };
    let Some(issuer) = settings.issuer()? else {
        bail!(
            "profile `{name}` has no Issuer ID and is not marked as an individual key; {login_hint}"
        );
    };
    let pem = match store.get(name) {
        Ok(pem) => pem,
        Err(err @ (AuthError::NotFound { .. } | AuthError::Corrupted { .. })) => {
            return Err(err.into());
        }
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context(format!("cannot load the API key of profile `{name}`")));
        }
    };
    Ok(StoredKey {
        settings,
        key_id,
        issuer,
        pem,
    })
}

#[cfg(test)]
mod tests {
    use appstore_connect_api::auth::MemoryStore;

    use super::*;
    use crate::testing::generated_key_pem;

    const ISSUER: &str = "01234567-89ab-cdef-0123-456789abcdef";

    fn sample() -> ProfileSettings {
        ProfileSettings {
            key_id: Some("TESTKEY123".into()),
            issuer_id: Some(ISSUER.into()),
            individual: false,
            vendor_number: Some("80000000".into()),
            client: Some("codex".into()),
            allow_write: true,
            allow_destructive: false,
            allow_path: vec![],
            toolsets: vec!["apps".into(), "testflight".into()],
            dynamic_toolsets: true,
        }
    }

    #[test]
    fn access_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let mut settings = sample();
        assert_eq!(settings.access().level, mcp_kit::AccessLevel::Write);
        let root = dir.path().join("files");
        settings.set_access(&ProfileAccess::new(
            mcp_kit::AccessLevel::Destructive,
            vec![root.clone()],
        ));
        assert!(settings.allow_write && settings.allow_destructive);
        repo.upsert("work", settings.clone()).unwrap();
        let text = fs::read_to_string(repo.path()).unwrap();
        assert!(text.contains("allow_path = ["), "{text}");
        let loaded = repo.get("work").unwrap().unwrap();
        assert_eq!(loaded.access().allow_path, vec![root]);
        assert_eq!(loaded, settings);
        settings.set_access(&ProfileAccess::read_only());
        assert!(!settings.allow_write && !settings.allow_destructive);
        assert!(settings.allow_path.is_empty());
        assert_eq!(repo.list().unwrap().len(), 1);
    }

    #[test]
    fn round_trip_in_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("nested"));
        assert_eq!(repo.get("codex").unwrap(), None);

        repo.upsert("codex", sample()).unwrap();
        repo.upsert(DEFAULT_PROFILE, ProfileSettings::default())
            .unwrap();
        assert_eq!(repo.get("codex").unwrap(), Some(sample()));
        assert_eq!(
            repo.get(DEFAULT_PROFILE).unwrap(),
            Some(ProfileSettings::default())
        );

        let text = fs::read_to_string(repo.path()).unwrap();
        assert!(text.contains("[profiles.codex]"), "{text}");
        assert!(text.contains("key_id = \"TESTKEY123\""), "{text}");
        assert!(text.contains("allow_write = true"), "{text}");
        assert!(text.contains("dynamic_toolsets = true"), "{text}");
        // Пустые необязательные поля и выключенные флаги не записываются.
        assert_eq!(text.matches("issuer_id").count(), 1, "{text}");
        assert!(!text.contains("individual"), "{text}");

        assert!(repo.remove("codex").unwrap());
        assert!(!repo.remove("codex").unwrap());
        assert_eq!(repo.get("codex").unwrap(), None);
        assert!(repo.get(DEFAULT_PROFILE).unwrap().is_some());
    }

    #[test]
    fn parses_hand_written_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        fs::write(
            repo.path(),
            "# comment\n[profiles.ci]\nkey_id = \"ABC\"\nindividual = true\nallow_write = true\n\n[profiles.empty]\n",
        )
        .unwrap();
        let ci = repo.get("ci").unwrap().unwrap();
        assert!(ci.allow_write);
        assert!(!ci.allow_destructive);
        assert!(ci.toolsets.is_empty());
        assert_eq!(ci.issuer().unwrap(), Some(Issuer::Individual));
        assert_eq!(ci.key_id().unwrap().unwrap().as_str(), "ABC");
        assert_eq!(repo.get("empty").unwrap(), Some(ProfileSettings::default()));
    }

    #[test]
    fn destructive_implies_write() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        fs::write(repo.path(), "[profiles.old]\nallow_destructive = true\n").unwrap();
        let old = repo.get("old").unwrap().unwrap();
        assert!(!old.allow_write);
        assert!(old.writes_allowed());

        let settings = ProfileSettings {
            allow_destructive: true,
            ..ProfileSettings::default()
        };
        repo.upsert("new", settings).unwrap();
        let saved = repo.get("new").unwrap().unwrap();
        assert!(saved.allow_write);
        assert!(saved.allow_destructive);
    }

    #[test]
    fn rejects_unknown_fields_and_bad_values() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        // Ключ в файле профилей не предусмотрен.
        fs::write(repo.path(), "[profiles.ci]\nprivate_key = \"x\"\n").unwrap();
        assert!(repo.get("ci").is_err());

        let both = ProfileSettings {
            issuer_id: Some(ISSUER.into()),
            individual: true,
            ..Default::default()
        };
        assert!(both.issuer().is_err());
        let bad = ProfileSettings {
            issuer_id: Some("nope".into()),
            key_id: Some("a b".into()),
            ..Default::default()
        };
        assert!(bad.issuer().is_err());
        assert!(bad.key_id().is_err());
    }

    #[test]
    fn validates_profile_names_and_vendor_numbers() {
        for good in ["default", "claude-code", "ci_1", "a.b"] {
            validate_profile_name(good).unwrap();
        }
        for bad in ["", "-x", "a b", "a/b", "проф", &"x".repeat(65)] {
            assert!(validate_profile_name(bad).is_err(), "{bad}");
        }
        assert_eq!(validate_vendor_number(" 80012345 ").unwrap(), "80012345");
        for bad in ["", "8001 2345", "abc", &"1".repeat(21)] {
            assert!(validate_vendor_number(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn set_issuer_switches_key_type() {
        let mut settings = sample();
        settings.set_issuer(&Issuer::Individual);
        assert_eq!(settings.issuer_id, None);
        assert!(settings.individual);
        settings.set_issuer(&Issuer::Team(IssuerId::new(ISSUER).unwrap()));
        assert_eq!(settings.issuer_id.as_deref(), Some(ISSUER));
        assert!(!settings.individual);
    }

    #[test]
    fn load_profile_requires_key_and_ids() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();

        let err = load_profile_with(&repo, &store, "codex").unwrap_err();
        assert!(err.to_string().contains("no Key ID"), "{err}");

        repo.upsert("codex", sample()).unwrap();
        let err = load_profile_with(&repo, &store, "codex").unwrap_err();
        assert!(
            err.to_string().contains("auth login --profile codex"),
            "{err}"
        );

        store
            .set(
                "codex",
                &PrivateKeyPem::parse(&generated_key_pem()).unwrap(),
            )
            .unwrap();
        let profile = load_profile_with(&repo, &store, "codex").unwrap();
        assert_eq!(profile.settings, sample());
        assert_eq!(profile.key.key_id().as_str(), "TESTKEY123");
        assert_eq!(profile.key.issuer().kind(), "team");

        let no_issuer = ProfileSettings {
            issuer_id: None,
            ..sample()
        };
        repo.upsert("codex", no_issuer).unwrap();
        let err = load_profile_with(&repo, &store, "codex").unwrap_err();
        assert!(err.to_string().contains("no Issuer ID"), "{err}");
    }
}
