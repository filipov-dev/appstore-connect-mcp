//! JWT для App Store Connect API (ES256).
//!
//! Токен выпускается в памяти процесса из ключа `.p8` ([`ApiKey`]) и нигде не сохраняется:
//! - заголовок: `alg = ES256`, `kid = <Key ID>`, `typ = JWT`;
//! - claims командного ключа: `iss = <Issuer ID>`, `iat`, `exp`, `aud = appstoreconnect-v1`;
//! - claims индивидуального ключа: `sub = user` вместо `iss`;
//! - `exp` не дальше 20 минут от `iat` (ограничение Apple); выпускаем на
//!   [`TOKEN_LIFETIME`] и заменяем заранее, когда до `exp` остаётся меньше
//!   [`REFRESH_MARGIN`], чтобы токен не истёк между выдачей и запросом (в том числе за
//!   время повторов после 429).
//!
//! Подпись детерминированная (RFC 6979), поэтому системный генератор случайных чисел для
//! неё не нужен. Время берётся из [`Clock`]: тесты подставляют [`ManualClock`].

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::Signature;
use p256::ecdsa::signature::Signer;
use serde::Serialize;
use zeroize::Zeroizing;

use crate::key::{ApiKey, Issuer, KeyError};

/// Значение claim `aud`.
pub const AUDIENCE: &str = "appstoreconnect-v1";

/// Срок жизни выпускаемого токена.
pub const TOKEN_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// Наибольший срок жизни токена, который принимает App Store Connect.
pub const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(20 * 60);

/// Токен заменяется новым, когда до `exp` остаётся меньше этого времени.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// Сколько прежних токенов помнить для вырезания из текстов ([`JwtTokens::known_tokens`]).
const REMEMBERED_TOKENS: usize = 4;

/// Источник текущего времени.
pub trait Clock: Send + Sync + fmt::Debug {
    fn now(&self) -> SystemTime;
}

/// Системные часы.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Управляемые часы для тестов: время меняется только вызовами [`ManualClock::advance`]
/// и [`ManualClock::set`].
#[derive(Debug)]
pub struct ManualClock(Mutex<SystemTime>);

impl ManualClock {
    pub fn new(start: SystemTime) -> Self {
        Self(Mutex::new(start))
    }

    pub fn set(&self, time: SystemTime) {
        *self.lock() = time;
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self.lock();
        *now += by;
    }

    fn lock(&self) -> MutexGuard<'_, SystemTime> {
        // Отравление невозможно: под блокировкой нет кода, который паникует.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.lock()
    }
}

/// Подписанный JWT. `Debug` и `Display` не раскрывают значение, память затирается при
/// освобождении.
#[derive(Clone, PartialEq, Eq)]
pub struct Jwt(Zeroizing<String>);

impl Jwt {
    /// Значение токена. Использовать только для заголовка `Authorization`.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Jwt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Jwt(***)")
    }
}

impl fmt::Display for Jwt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[derive(Serialize)]
struct Header<'a> {
    alg: &'static str,
    kid: &'a str,
    typ: &'static str,
}

#[derive(Serialize)]
struct Claims<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    iss: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sub: Option<&'static str>,
    iat: u64,
    exp: u64,
    aud: &'static str,
}

/// Подписывает JWT ключом `key` со временем выпуска `issued_at` и окончания `expires_at`
/// (секунды Unix).
pub fn sign(key: &ApiKey, issued_at: u64, expires_at: u64) -> Jwt {
    let header = Header {
        alg: "ES256",
        kid: key.key_id().as_str(),
        typ: "JWT",
    };
    let (iss, sub) = match key.issuer() {
        Issuer::Team(issuer) => (Some(issuer.as_str()), None),
        Issuer::Individual => (None, Some("user")),
    };
    let claims = Claims {
        iss,
        sub,
        iat: issued_at,
        exp: expires_at,
        aud: AUDIENCE,
    };
    // Сериализация структур из строк и чисел не может завершиться ошибкой.
    let header = serde_json::to_vec(&header).unwrap_or_default();
    let claims = serde_json::to_vec(&claims).unwrap_or_default();
    let mut token = Zeroizing::new(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(claims)
    ));
    // ES256 в JWS — подпись r||s фиксированной длины (64 байта), не DER.
    let signature: Signature = key.signing_key().sign(token.as_bytes());
    token.push('.');
    token.push_str(&URL_SAFE_NO_PAD.encode(signature.to_bytes()));
    Jwt(token)
}

#[derive(Clone)]
struct Cached {
    jwt: Jwt,
    issued_at: u64,
    expires_at: u64,
}

#[derive(Default)]
struct State {
    current: Option<Cached>,
    /// Недавние прежние токены (новые в конце).
    previous: Vec<Jwt>,
}

/// Источник JWT для исполнителя: выпускает токен по ключу, держит его в памяти и
/// заменяет заранее (см. описание модуля).
///
/// `Debug` не раскрывает ни ключ, ни токены.
pub struct JwtTokens {
    key: ApiKey,
    clock: Arc<dyn Clock>,
    lifetime: Duration,
    refresh_margin: Duration,
    state: Mutex<State>,
}

impl fmt::Debug for JwtTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JwtTokens")
            .field("key", &self.key)
            .field("lifetime", &self.lifetime)
            .field("refresh_margin", &self.refresh_margin)
            .finish_non_exhaustive()
    }
}

impl JwtTokens {
    /// Источник токенов с системными часами.
    pub fn new(key: ApiKey) -> Self {
        Self::with_clock(key, Arc::new(SystemClock))
    }

    /// Источник токенов с заданными часами (тесты).
    pub fn with_clock(key: ApiKey, clock: Arc<dyn Clock>) -> Self {
        Self {
            key,
            clock,
            lifetime: TOKEN_LIFETIME,
            refresh_margin: REFRESH_MARGIN,
            state: Mutex::new(State::default()),
        }
    }

    pub fn key(&self) -> &ApiKey {
        &self.key
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Отравление невозможно: под блокировкой нет кода, который паникует.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Действующий токен: из памяти, если до его `exp` больше [`REFRESH_MARGIN`], иначе
    /// новый.
    pub fn token(&self) -> Result<Jwt, KeyError> {
        let now = unix_seconds(self.clock.now())?;
        let mut state = self.lock();
        if let Some(cached) = &state.current
            // Часы отвели назад раньше `iat`: такой токен Apple может счесть выпущенным
            // в будущем, выпускаем заново.
            && now >= cached.issued_at
            && now.saturating_add(self.refresh_margin.as_secs()) < cached.expires_at
        {
            return Ok(cached.jwt.clone());
        }
        let expires_at = now.saturating_add(self.lifetime.as_secs());
        let jwt = sign(&self.key, now, expires_at);
        if let Some(old) = state.current.take() {
            state.previous.push(old.jwt);
            if state.previous.len() > REMEMBERED_TOKENS {
                state.previous.remove(0);
            }
        }
        state.current = Some(Cached {
            jwt: jwt.clone(),
            issued_at: now,
            expires_at,
        });
        Ok(jwt)
    }

    /// Текущий и недавние прежние токены: исполнитель вырезает их из текстов ошибок и
    /// ответов на случай, если сервер отразил заголовок авторизации.
    pub fn known_tokens(&self) -> Vec<Jwt> {
        let state = self.lock();
        state
            .previous
            .iter()
            .cloned()
            .chain(state.current.iter().map(|cached| cached.jwt.clone()))
            .collect()
    }
}

fn unix_seconds(time: SystemTime) -> Result<u64, KeyError> {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| KeyError::Clock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_on_demand() {
        let start = UNIX_EPOCH + Duration::from_secs(10);
        let clock = ManualClock::new(start);
        assert_eq!(clock.now(), start);
        clock.advance(Duration::from_secs(5));
        assert_eq!(clock.now(), start + Duration::from_secs(5));
        clock.set(start);
        assert_eq!(clock.now(), start);
    }

    #[test]
    fn lifetime_limits_are_consistent() {
        assert!(TOKEN_LIFETIME <= MAX_TOKEN_LIFETIME);
        assert!(REFRESH_MARGIN < TOKEN_LIFETIME);
    }

    #[test]
    fn jwt_debug_hides_value() {
        let jwt = Jwt(Zeroizing::new("a.b.c".into()));
        assert_eq!(format!("{jwt:?}"), "Jwt(***)");
        assert_eq!(jwt.to_string(), "***");
        assert_eq!(jwt.expose(), "a.b.c");
    }
}
