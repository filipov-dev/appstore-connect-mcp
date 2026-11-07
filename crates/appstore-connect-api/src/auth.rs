//! Хранение ключей App Store Connect API в системном хранилище.
//!
//! Каждый профиль (один профиль — один кабинет) хранит свой ключ `.p8` отдельной записью:
//! сервис [`KEYRING_SERVICE`], имя учётной записи — имя профиля. Значение записи — PEM-текст
//! ключа ([`PrivateKeyPem`]); Key ID, Issuer ID и прочие несекретные настройки лежат в файле
//! профилей CLI.
//!
//! Запасного файлового хранилища нет: если системное хранилище недоступно (Linux без
//! Secret Service), операции завершаются ошибкой [`AuthError::Unavailable`] с подсказкой.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use crate::key::PrivateKeyPem;

/// Имя сервиса записей в Keychain / Credential Manager / Secret Service.
pub const KEYRING_SERVICE: &str = "dev.filipov.appstore-connect-mcp";

/// Системное хранилище на этой платформе — Secret Service (D-Bus). На Linux и BSD keyring
/// работает только через него, и «хранилище недоступно» там почти всегда означает, что в
/// сессии нет демона Secret Service.
const USES_SECRET_SERVICE: bool = cfg!(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd"
));

/// Подсказка для Linux без Secret Service.
const SECRET_SERVICE_HINT: &str = "no Secret Service (org.freedesktop.secrets) is available \
on the D-Bus session bus. appstore-connect-mcp stores API keys only in the system keychain. \
Run it in a desktop session with a keyring daemon (GNOME Keyring, KDE Wallet, KeePassXC with \
Secret Service integration). On a headless machine start a session bus and an unlocked keyring \
first, for example: dbus-run-session -- sh, then inside it \
gnome-keyring-daemon --unlock --components=secrets (reads the keyring password from stdin)";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(
        "no App Store Connect API key is stored for profile `{profile}`; store one with \
         `appstore-connect-mcp auth login --profile {profile} --key-file <AuthKey_XXXXXXXXXX.p8>`"
    )]
    NotFound { profile: String },
    /// В хранилище лежит значение, которое не является ключом `.p8`.
    #[error(
        "the value stored for profile `{profile}` is not a valid API key; store the key again \
         with `appstore-connect-mcp auth login --profile {profile} --key-file <AuthKey_XXXXXXXXXX.p8>`"
    )]
    Corrupted { profile: String },
    #[error("the system keychain is not available: {0}")]
    Unavailable(String),
    #[error("system keychain error: {0}")]
    Backend(String),
}

/// Хранилище ключей по профилям.
pub trait KeyStore: Send + Sync {
    fn get(&self, profile: &str) -> Result<PrivateKeyPem, AuthError>;
    fn set(&self, profile: &str, key: &PrivateKeyPem) -> Result<(), AuthError>;
    fn delete(&self, profile: &str) -> Result<(), AuthError>;

    /// Есть ли ключ у профиля. Значение наружу не возвращается; запись, которая не
    /// разбирается как ключ, тоже считается существующей.
    fn contains(&self, profile: &str) -> Result<bool, AuthError> {
        match self.get(profile) {
            Ok(_) | Err(AuthError::Corrupted { .. }) => Ok(true),
            Err(AuthError::NotFound { .. }) => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// Хранилище на базе системного keyring (Keychain на macOS, Credential Manager на Windows,
/// Secret Service на Linux).
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringStore;

impl KeyringStore {
    fn entry(profile: &str) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(KEYRING_SERVICE, profile).map_err(|err| map_error(profile, err))
    }
}

/// Переводит ошибку keyring в [`AuthError`]. Значение ключа в текст ошибки не попадает.
fn map_error(profile: &str, err: keyring::Error) -> AuthError {
    map_error_for(profile, err, USES_SECRET_SERVICE)
}

/// [`map_error`] с явной платформой: так подсказку для Secret Service можно проверить в
/// тестах на любой ОС.
fn map_error_for(profile: &str, err: keyring::Error, secret_service: bool) -> AuthError {
    match err {
        keyring::Error::NoEntry => AuthError::NotFound {
            profile: profile.to_owned(),
        },
        // `BadEncoding` содержит байты сохранённого значения — их не выводим.
        keyring::Error::BadEncoding(_) => AuthError::Corrupted {
            profile: profile.to_owned(),
        },
        // Нет сессионной шины D-Bus, нет демона Secret Service или хранилище заблокировано.
        keyring::Error::NoStorageAccess(details) | keyring::Error::PlatformFailure(details)
            if secret_service =>
        {
            AuthError::Unavailable(format!("{SECRET_SERVICE_HINT}. Details: {details}"))
        }
        keyring::Error::NoStorageAccess(details) => AuthError::Unavailable(details.to_string()),
        other => AuthError::Backend(other.to_string()),
    }
}

impl KeyStore for KeyringStore {
    fn get(&self, profile: &str) -> Result<PrivateKeyPem, AuthError> {
        let password = zeroize::Zeroizing::new(
            Self::entry(profile)?
                .get_password()
                .map_err(|err| map_error(profile, err))?,
        );
        PrivateKeyPem::parse(&password).map_err(|_| AuthError::Corrupted {
            profile: profile.to_owned(),
        })
    }

    fn set(&self, profile: &str, key: &PrivateKeyPem) -> Result<(), AuthError> {
        Self::entry(profile)?
            .set_password(key.expose())
            .map_err(|err| map_error(profile, err))
    }

    fn delete(&self, profile: &str) -> Result<(), AuthError> {
        Self::entry(profile)?
            .delete_credential()
            .map_err(|err| map_error(profile, err))
    }
}

/// Имя сервиса записей с секретами, которые создают инструменты сервера (приватные ключи
/// сертификатов, пароли `.p12`). Отдельно от [`KEYRING_SERVICE`]: ключи API и эти секреты
/// не смешиваются, и `auth logout` их не трогает.
pub const SECRETS_KEYRING_SERVICE: &str = "dev.filipov.appstore-connect-mcp.secrets";

/// Хранилище секретов профиля в системном keyring: сервис [`SECRETS_KEYRING_SERVICE`],
/// учётная запись `<профиль>/<имя записи>`. Значения только записываются и проверяются на
/// наличие: наружу (в ответы инструментов) они не отдаются.
#[derive(Debug, Clone)]
pub struct KeyringVault {
    profile: String,
}

impl KeyringVault {
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
        }
    }

    /// Имя учётной записи keyring для записи `name`.
    pub fn account(&self, name: &str) -> String {
        format!("{}/{name}", self.profile)
    }

    fn entry(&self, name: &str) -> Result<(keyring::Entry, String), AuthError> {
        let account = self.account(name);
        let entry = keyring::Entry::new(SECRETS_KEYRING_SERVICE, &account)
            .map_err(|err| map_error(&account, err))?;
        Ok((entry, account))
    }

    /// Сохраняет значение записи, заменяя прежнее.
    pub fn store(&self, name: &str, value: &str) -> Result<(), AuthError> {
        let (entry, account) = self.entry(name)?;
        entry
            .set_password(value)
            .map_err(|err| map_error(&account, err))
    }

    /// Есть ли запись. Значение не возвращается.
    pub fn contains(&self, name: &str) -> Result<bool, AuthError> {
        let (entry, account) = self.entry(name)?;
        match entry.get_password() {
            Ok(value) => {
                drop(zeroize::Zeroizing::new(value));
                Ok(true)
            }
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(keyring::Error::BadEncoding(_)) => Ok(true),
            Err(err) => Err(map_error(&account, err)),
        }
    }
}

/// Хранилище в памяти для тестов: системный keychain в автотестах не используется.
#[derive(Debug, Default)]
pub struct MemoryStore {
    keys: Mutex<HashMap<String, PrivateKeyPem>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, PrivateKeyPem>>, AuthError> {
        self.keys
            .lock()
            .map_err(|_| AuthError::Backend("the in-memory key store is poisoned".into()))
    }
}

impl KeyStore for MemoryStore {
    fn get(&self, profile: &str) -> Result<PrivateKeyPem, AuthError> {
        self.lock()?
            .get(profile)
            .cloned()
            .ok_or_else(|| AuthError::NotFound {
                profile: profile.to_owned(),
            })
    }

    fn set(&self, profile: &str, key: &PrivateKeyPem) -> Result<(), AuthError> {
        self.lock()?.insert(profile.to_owned(), key.clone());
        Ok(())
    }

    fn delete(&self, profile: &str) -> Result<(), AuthError> {
        self.lock()?
            .remove(profile)
            .map(|_| ())
            .ok_or_else(|| AuthError::NotFound {
                profile: profile.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use p256::ecdsa::SigningKey;
    use p256::elliptic_curve::Generate;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};

    use super::*;

    fn key() -> PrivateKeyPem {
        let pem = SigningKey::generate().to_pkcs8_pem(LineEnding::LF).unwrap();
        PrivateKeyPem::parse(&pem).unwrap()
    }

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryStore::new();
        assert!(!store.contains("codex").unwrap());
        assert!(matches!(
            store.get("codex"),
            Err(AuthError::NotFound { profile }) if profile == "codex"
        ));

        let (k1, k2, k3) = (key(), key(), key());
        store.set("codex", &k1).unwrap();
        store.set("claude-code", &k2).unwrap();
        assert_eq!(store.get("codex").unwrap(), k1);
        assert!(store.contains("claude-code").unwrap());

        store.set("codex", &k3).unwrap();
        assert_eq!(store.get("codex").unwrap(), k3);

        store.delete("codex").unwrap();
        assert!(!store.contains("codex").unwrap());
        assert!(matches!(
            store.delete("codex"),
            Err(AuthError::NotFound { .. })
        ));
        assert_eq!(store.get("claude-code").unwrap(), k2);
    }

    #[test]
    fn keyring_errors_are_mapped_without_values() {
        assert!(matches!(
            map_error("p", keyring::Error::NoEntry),
            AuthError::NotFound { profile } if profile == "p"
        ));
        let err = map_error("p", keyring::Error::BadEncoding(b"secret-bytes".to_vec()));
        assert!(matches!(err, AuthError::Corrupted { .. }), "{err:?}");
        assert!(!err.to_string().contains("secret"));
        let err = AuthError::NotFound {
            profile: "ci".into(),
        };
        assert!(err.to_string().contains("auth login --profile ci"), "{err}");
    }

    #[test]
    fn missing_secret_service_gets_a_hint() {
        let no_bus = || keyring::Error::PlatformFailure("no session bus".into());
        let err = map_error_for("p", no_bus(), true);
        assert!(matches!(err, AuthError::Unavailable(_)), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("Secret Service"), "{text}");
        assert!(text.contains("dbus-run-session"), "{text}");
        assert!(text.contains("no session bus"), "{text}");

        let err = map_error_for("p", keyring::Error::NoStorageAccess("locked".into()), true);
        assert!(err.to_string().contains("Secret Service"), "{err}");

        // На macOS и Windows подсказки про D-Bus нет.
        let err = map_error_for("p", no_bus(), false);
        assert!(matches!(err, AuthError::Backend(_)), "{err:?}");
        assert!(!err.to_string().contains("Secret Service"), "{err}");
        let err = map_error_for("p", keyring::Error::NoStorageAccess("denied".into()), false);
        assert!(matches!(err, AuthError::Unavailable(_)), "{err:?}");
    }

    #[test]
    fn vault_accounts_are_per_profile() {
        let vault = KeyringVault::new("work");
        assert_eq!(vault.account("cert-ABC.key"), "work/cert-ABC.key");
        assert_ne!(SECRETS_KEYRING_SERVICE, KEYRING_SERVICE);
    }

    /// Проверка с настоящим системным хранилищем: запускать вручную (`cargo test -- --ignored`).
    #[test]
    #[ignore = "uses the real system keychain"]
    fn keyring_store_round_trip() {
        let store = KeyringStore;
        let profile = "appstore-connect-mcp-test-profile";
        let _ = store.delete(profile);
        assert!(!store.contains(profile).unwrap());
        let value = key();
        store.set(profile, &value).unwrap();
        assert_eq!(store.get(profile).unwrap(), value);
        store.delete(profile).unwrap();
        assert!(!store.contains(profile).unwrap());
    }
}
