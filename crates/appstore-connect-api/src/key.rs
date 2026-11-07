//! Ключ App Store Connect API (`.p8`) и данные, с которыми по нему выпускается JWT.
//!
//! Ключ — приватный ключ EC P-256 в PEM-файле PKCS#8 (`AuthKey_<KEYID>.p8`, начинается
//! со строки `BEGIN PRIVATE KEY` между пятью дефисами). Он хранится в системном хранилище ([`crate::auth`]),
//! в память процесса попадает только как [`PrivateKeyPem`] и [`ApiKey`]: их `Debug` не
//! раскрывает значение, а память PEM-текста затирается при освобождении.
//!
//! Key ID и Issuer ID секретами не являются, но относятся к конкретному кабинету, поэтому
//! их `Debug` тоже скрыт: они не должны попадать в логи. Показывать их можно только
//! пользователю в его терминале (`auth status`).

use std::fmt;
use std::path::Path;

use p256::ecdsa::SigningKey;
use p256::pkcs8::DecodePrivateKey;
use zeroize::Zeroizing;

/// Предел размера файла ключа. Настоящий `.p8` — около 250 байт.
pub const MAX_KEY_FILE_BYTES: usize = 16 * 1024;

/// Первая строка PEM-файла PKCS#8.
const PEM_BEGIN: &str = concat!("-----", "BEGIN PRIVATE KEY", "-----");
/// Последняя строка PEM-файла PKCS#8.
const PEM_END: &str = concat!("-----", "END PRIVATE KEY", "-----");

/// Предел длины Key ID. У Apple он 10 символов; запас — на случай смены формата.
const MAX_KEY_ID_LEN: usize = 32;

/// Ошибки разбора ключа и его параметров. Тексты не содержат ни ключа, ни его частей.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error(
        "the API key is not a valid App Store Connect private key: {0}. Expected the \
         AuthKey_XXXXXXXXXX.p8 file downloaded from App Store Connect (PKCS#8 PEM with an EC \
         P-256 key, starting with the `BEGIN PRIVATE KEY` header line)"
    )]
    InvalidKey(String),
    #[error(
        "invalid Key ID: expected the ID of the key shown in App Store Connect (Users and \
         Access > Integrations), up to {MAX_KEY_ID_LEN} letters and digits, for example the \
         XXXXXXXXXX part of AuthKey_XXXXXXXXXX.p8"
    )]
    InvalidKeyId,
    #[error(
        "invalid Issuer ID: expected a UUID like 01234567-89ab-cdef-0123-456789abcdef, shown \
         above the list of team keys in App Store Connect (Users and Access > Integrations)"
    )]
    InvalidIssuerId,
    /// Системные часы раньше 1970 года: JWT с такими `iat`/`exp` не выпустить.
    #[error("the system clock is set before 1970, cannot issue a JWT")]
    Clock,
}

/// PEM-текст приватного ключа `.p8`.
///
/// Создаётся только через [`PrivateKeyPem::parse`], поэтому всегда содержит корректный
/// ключ P-256. `Debug` не раскрывает значение, память затирается при освобождении.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateKeyPem(Zeroizing<String>);

impl PrivateKeyPem {
    /// Проверяет текст файла `.p8` и приводит его к единому виду: без BOM, с переводами
    /// строк `\n` и одним `\n` в конце. Файл должен содержать ровно один блок PKCS#8.
    pub fn parse(text: &str) -> Result<Self, KeyError> {
        if text.len() > MAX_KEY_FILE_BYTES {
            return Err(KeyError::InvalidKey(format!(
                "the file is larger than {MAX_KEY_FILE_BYTES} bytes"
            )));
        }
        let normalized = Zeroizing::new(text.trim_start_matches('\u{feff}').replace("\r\n", "\n"));
        let trimmed = normalized.trim();
        if trimmed.contains("ENCRYPTED PRIVATE KEY") {
            return Err(KeyError::InvalidKey(
                "the key is encrypted with a password".into(),
            ));
        }
        if trimmed.contains("BEGIN EC PRIVATE KEY") {
            return Err(KeyError::InvalidKey(
                "the key is in SEC1 format (`BEGIN EC PRIVATE KEY`), not PKCS#8".into(),
            ));
        }
        if !trimmed.starts_with(PEM_BEGIN) || !trimmed.ends_with(PEM_END) {
            return Err(KeyError::InvalidKey(
                "the text is not a single PEM block `PRIVATE KEY`".into(),
            ));
        }
        if trimmed.matches(PEM_BEGIN).count() != 1 {
            return Err(KeyError::InvalidKey(
                "the file contains more than one key".into(),
            ));
        }
        let pem = Zeroizing::new(format!("{trimmed}\n"));
        // Описание ошибки разбора — только вид ошибки ASN.1/PEM, без байтов ключа.
        SigningKey::from_pkcs8_pem(&pem).map_err(|err| {
            KeyError::InvalidKey(format!("it is not an EC P-256 PKCS#8 key ({err})"))
        })?;
        Ok(Self(pem))
    }

    /// PEM-текст. Использовать только для записи в системное хранилище и передачи ключа
    /// дочерней команде (`key exec`); не выводить и не логировать.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Ключ подписи ES256.
    pub fn signing_key(&self) -> Result<SigningKey, KeyError> {
        SigningKey::from_pkcs8_pem(&self.0).map_err(|err| {
            KeyError::InvalidKey(format!("it is not an EC P-256 PKCS#8 key ({err})"))
        })
    }
}

impl fmt::Debug for PrivateKeyPem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivateKeyPem(***)")
    }
}

/// Key ID ключа API: заголовок `kid` JWT.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyId(String);

impl KeyId {
    /// Проверяет Key ID: 1–32 латинские буквы и цифры (у Apple — 10 символов).
    pub fn new(value: &str) -> Result<Self, KeyError> {
        let value = value.trim();
        let valid = !value.is_empty()
            && value.len() <= MAX_KEY_ID_LEN
            && value.chars().all(|c| c.is_ascii_alphanumeric());
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(KeyError::InvalidKeyId)
        }
    }

    /// Key ID из имени файла, которое даёт App Store Connect: `AuthKey_<KEYID>.p8`.
    pub fn from_file_name(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        let id = name.strip_prefix("AuthKey_")?.strip_suffix(".p8")?;
        Self::new(id).ok()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyId(***)")
    }
}

/// Issuer ID командного ключа: claim `iss` JWT.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuerId(String);

impl IssuerId {
    /// Проверяет Issuer ID: UUID в виде `8-4-4-4-12` шестнадцатеричных цифр.
    pub fn new(value: &str) -> Result<Self, KeyError> {
        let value = value.trim();
        let valid = value.len() == 36
            && value.char_indices().all(|(index, c)| match index {
                8 | 13 | 18 | 23 => c == '-',
                _ => c.is_ascii_hexdigit(),
            });
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(KeyError::InvalidIssuerId)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for IssuerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IssuerId(***)")
    }
}

/// Тип ключа API: от него зависят claims JWT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issuer {
    /// Командный ключ (Team key): `iss` = Issuer ID.
    Team(IssuerId),
    /// Индивидуальный ключ (Individual key): `sub` = `user`, без `iss`.
    Individual,
}

impl Issuer {
    /// Название типа для пользователя: `team` или `individual`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Team(_) => "team",
            Self::Individual => "individual",
        }
    }
}

/// Ключ API, готовый к выпуску JWT: ключ подписи, Key ID и тип ключа.
///
/// `Debug` показывает только тип ключа.
#[derive(Clone)]
pub struct ApiKey {
    key_id: KeyId,
    issuer: Issuer,
    signing_key: SigningKey,
}

impl ApiKey {
    pub fn new(pem: &PrivateKeyPem, key_id: KeyId, issuer: Issuer) -> Result<Self, KeyError> {
        Ok(Self {
            key_id,
            issuer,
            signing_key: pem.signing_key()?,
        })
    }

    pub fn key_id(&self) -> &KeyId {
        &self.key_id
    }

    pub fn issuer(&self) -> &Issuer {
        &self.issuer
    }

    pub(crate) fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKey")
            .field("key_id", &self.key_id)
            .field("issuer", &self.issuer.kind())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use p256::elliptic_curve::Generate;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};

    use super::*;

    fn generated_pem() -> String {
        SigningKey::generate()
            .to_pkcs8_pem(LineEnding::LF)
            .unwrap()
            .to_string()
    }

    #[test]
    fn parses_and_normalizes_pkcs8_pem() {
        let pem = generated_pem();
        let parsed = PrivateKeyPem::parse(&pem).unwrap();
        assert_eq!(parsed.expose(), pem);

        // CRLF, BOM, отступы и отсутствие перевода строки в конце — тот же ключ.
        let messy = format!("\u{feff}  {}  ", pem.trim().replace('\n', "\r\n"));
        assert_eq!(PrivateKeyPem::parse(&messy).unwrap(), parsed);
        parsed.signing_key().unwrap();
    }

    #[test]
    fn rejects_other_texts_without_echoing_them() {
        let pem = generated_pem();
        let body: String = pem.lines().nth(1).unwrap().to_owned();
        let sec1 = concat!(
            "-----",
            "BEGIN EC PRIVATE KEY-----\nAAAA\n-----",
            "END EC PRIVATE KEY-----\n"
        );
        let encrypted = concat!(
            "-----",
            "BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----",
            "END ENCRYPTED PRIVATE KEY-----\n"
        );
        let broken = pem.replace(&body, &body.chars().rev().collect::<String>());
        let two = format!("{pem}{pem}");
        for (text, expected) in [
            ("", "single PEM block"),
            ("hello", "single PEM block"),
            (sec1, "SEC1"),
            (encrypted, "encrypted"),
            (two.as_str(), "more than one key"),
            (broken.as_str(), "not an EC P-256 PKCS#8 key"),
        ] {
            let err = PrivateKeyPem::parse(text).unwrap_err().to_string();
            assert!(err.contains(expected), "{expected}: {err}");
            assert!(!err.contains(&body), "{err}");
        }
        let huge = "x".repeat(MAX_KEY_FILE_BYTES + 1);
        assert!(PrivateKeyPem::parse(&huge).is_err());
    }

    #[test]
    fn debug_hides_values() {
        let pem = PrivateKeyPem::parse(&generated_pem()).unwrap();
        assert_eq!(format!("{pem:?}"), "PrivateKeyPem(***)");
        let key = ApiKey::new(
            &pem,
            KeyId::new("TESTKEY123").unwrap(),
            Issuer::Team(IssuerId::new("01234567-89ab-cdef-0123-456789abcdef").unwrap()),
        )
        .unwrap();
        let debug = format!("{key:?}");
        assert!(!debug.contains("TESTKEY123"), "{debug}");
        assert!(!debug.contains("01234567"), "{debug}");
        assert!(debug.contains("team"), "{debug}");
    }

    #[test]
    fn key_id_rules() {
        assert_eq!(KeyId::new(" ABC123XYZ9 ").unwrap().as_str(), "ABC123XYZ9");
        for bad in ["", "a b", "ключ", "AB-12", &"A".repeat(33)] {
            assert_eq!(KeyId::new(bad), Err(KeyError::InvalidKeyId), "{bad}");
        }
        assert_eq!(
            KeyId::from_file_name(Path::new("/tmp/AuthKey_ABC123XYZ9.p8"))
                .unwrap()
                .as_str(),
            "ABC123XYZ9"
        );
        assert_eq!(KeyId::from_file_name(Path::new("key.p8")), None);
        assert_eq!(KeyId::from_file_name(Path::new("AuthKey_.p8")), None);
        assert_eq!(KeyId::from_file_name(Path::new("AuthKey_A B.p8")), None);
    }

    #[test]
    fn issuer_id_rules() {
        IssuerId::new("01234567-89ab-cdef-0123-456789ABCDEF").unwrap();
        for bad in [
            "",
            "01234567-89ab-cdef-0123-456789abcde",
            "01234567x89ab-cdef-0123-456789abcdef",
            "g1234567-89ab-cdef-0123-456789abcdef",
            "0123456789abcdef0123456789abcdef----",
        ] {
            assert_eq!(IssuerId::new(bad), Err(KeyError::InvalidIssuerId), "{bad}");
        }
    }
}
