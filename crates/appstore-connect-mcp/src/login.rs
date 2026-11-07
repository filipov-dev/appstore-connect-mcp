//! Приём нового ключа API: общая часть `auth login` и `connect`.
//!
//! Ключ `.p8` принимается только путём к файлу (флагом или ответом на вопрос), не
//! содержимым в аргументе или переменной окружения. Он проверяется (PKCS#8, EC P-256),
//! по умолчанию проверяется запросом `GET /v1/apps?limit=1` и сохраняется только в
//! системном хранилище.
//!
//! Удалить исходный файл предлагает вызывающая команда ([`offer_to_delete_key_file`]),
//! один раз и в самом конце: так вопрос не повторяется, если команда делает несколько шагов.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use appstore_connect_api::auth::{KEYRING_SERVICE, KeyStore};
use appstore_connect_api::key::MAX_KEY_FILE_BYTES;
use appstore_connect_api::{ApiKey, Issuer, IssuerId, KeyId, PrivateKeyPem};

use crate::profiles::ProfileSettings;
use crate::ui::Ui;
use crate::verify::KeyVerifier;

/// Флаги ключа из командной строки (`--key-file`, `--key-id`, `--issuer-id`, `--individual`).
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyInput<'a> {
    pub key_file: Option<&'a Path>,
    pub key_id: Option<&'a str>,
    pub issuer_id: Option<&'a str>,
    pub individual: bool,
}

impl KeyInput<'_> {
    /// Проверяет формат флагов до чтения файла: ошибка в них не должна стоить лишнего вопроса.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(id) = self.key_id {
            KeyId::new(id)?;
        }
        if let Some(id) = self.issuer_id {
            IssuerId::new(id)?;
        }
        Ok(())
    }
}

/// Новый ключ, сохранённый в системном хранилище.
#[derive(Debug)]
pub struct NewKey {
    pub key: ApiKey,
    /// Исходный файл ключа: его можно предложить удалить.
    pub file: PathBuf,
}

/// Путь из ответа пользователя: без кавычек, которые терминал добавляет при
/// перетаскивании файла, с `\ ` вместо пробелов и `~/` в начале.
pub fn parse_path_answer(answer: &str) -> Option<PathBuf> {
    let mut text = answer.trim();
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            text = &text[1..text.len() - 1];
        }
    }
    let text = text.replace("\\ ", " ");
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return Some(home.join(rest));
    }
    Some(PathBuf::from(text))
}

/// Читает и проверяет файл ключа. Содержимое в ошибки не попадает.
pub fn read_key_file(path: &Path) -> anyhow::Result<PrivateKeyPem> {
    let display = path.display();
    let file =
        fs::File::open(path).with_context(|| format!("cannot open the key file `{display}`"))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot read the key file `{display}`"))?;
    if !metadata.is_file() {
        bail!("`{display}` is not a regular file");
    }
    let mut text = zeroize::Zeroizing::new(String::new());
    file.take(MAX_KEY_FILE_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("cannot read the key file `{display}` as text"))?;
    PrivateKeyPem::parse(&text).with_context(|| format!("`{display}` is not a usable API key"))
}

/// Файл ключа: флаг, иначе вопрос.
fn resolve_key_file(ui: &mut dyn Ui, input: &KeyInput<'_>) -> anyhow::Result<PathBuf> {
    if let Some(path) = input.key_file {
        return Ok(path.to_path_buf());
    }
    let answer = ui
        .ask("Path to the API key file (AuthKey_XXXXXXXXXX.p8): ")
        .context("failed to read the key file path; pass --key-file <PATH>")?;
    parse_path_answer(&answer).context("no key file given; pass --key-file <PATH>")
}

/// Key ID нового ключа: флаг, иначе из имени файла, иначе вопрос.
fn resolve_new_key_id(
    ui: &mut dyn Ui,
    input: &KeyInput<'_>,
    key_file: &Path,
) -> anyhow::Result<KeyId> {
    if let Some(id) = input.key_id {
        return Ok(KeyId::new(id)?);
    }
    if let Some(id) = KeyId::from_file_name(key_file) {
        ui.say(&format!("Key ID: {} (from the file name).", id.as_str()));
        return Ok(id);
    }
    ask_key_id(ui)
}

/// Key ID уже сохранённого ключа: флаг, иначе из профиля, иначе вопрос.
pub fn resolve_stored_key_id(
    ui: &mut dyn Ui,
    input: &KeyInput<'_>,
    settings: &ProfileSettings,
) -> anyhow::Result<KeyId> {
    if let Some(id) = input.key_id {
        return Ok(KeyId::new(id)?);
    }
    if let Some(id) = settings.key_id()? {
        return Ok(id);
    }
    ask_key_id(ui)
}

fn ask_key_id(ui: &mut dyn Ui) -> anyhow::Result<KeyId> {
    let answer = ui
        .ask("Key ID (shown next to the key in App Store Connect): ")
        .context("failed to read the Key ID; pass --key-id <ID>")?;
    Ok(KeyId::new(&answer)?)
}

/// Тип ключа: флаги, иначе из профиля, иначе вопрос (пустой ответ — индивидуальный ключ).
pub fn resolve_issuer(
    ui: &mut dyn Ui,
    input: &KeyInput<'_>,
    settings: &ProfileSettings,
) -> anyhow::Result<Issuer> {
    if input.individual {
        return Ok(Issuer::Individual);
    }
    if let Some(id) = input.issuer_id {
        return Ok(Issuer::Team(IssuerId::new(id)?));
    }
    if let Some(issuer) = settings.issuer()? {
        match &issuer {
            Issuer::Team(id) => ui.say(&format!("Issuer ID: {} (from the profile).", id.as_str())),
            Issuer::Individual => ui.say("Key type: individual (from the profile)."),
        }
        return Ok(issuer);
    }
    let answer = ui
        .ask(
            "Issuer ID of the team key (Users and Access > Integrations; leave empty for an \
             individual key): ",
        )
        .context("failed to read the Issuer ID; pass --issuer-id <UUID> or --individual")?;
    if answer.trim().is_empty() {
        Ok(Issuer::Individual)
    } else {
        Ok(Issuer::Team(IssuerId::new(&answer)?))
    }
}

/// Читает новый ключ из файла, проверяет его (если `verifier` задан) и сохраняет в системном
/// хранилище. Если проверка не прошла, ничего не сохраняется.
pub async fn store_new_key<V: KeyVerifier>(
    store: &dyn KeyStore,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    profile: &str,
    input: &KeyInput<'_>,
    settings: &ProfileSettings,
) -> anyhow::Result<NewKey> {
    let file = resolve_key_file(ui, input)?;
    let pem = read_key_file(&file)?;
    let key_id = resolve_new_key_id(ui, input, &file)?;
    let issuer = resolve_issuer(ui, input, settings)?;
    let key = ApiKey::new(&pem, key_id, issuer)?;

    match verifier {
        Some(verifier) => {
            let outcome = verifier.check(&key).await.map_err(|err| {
                anyhow::anyhow!(
                    "{err:#} Nothing was stored. Re-run with --no-verify to store the key \
                     without verification."
                )
            })?;
            ui.say(&format!("Key is valid: {outcome}."));
        }
        None => ui.say("Skipping the key check (--no-verify)."),
    }

    let replaced = store.contains(profile)?;
    store.set(profile, &pem)?;
    ui.say(&format!(
        "API key {} in the system keychain (service `{KEYRING_SERVICE}`, account `{profile}`).",
        if replaced { "replaced" } else { "saved" }
    ));
    Ok(NewKey { key, file })
}

/// Записывает Key ID и тип ключа в настройки профиля.
pub fn remember_key(settings: &mut ProfileSettings, key: &ApiKey) {
    settings.key_id = Some(key.key_id().as_str().to_owned());
    settings.set_issuer(key.issuer());
}

/// Предлагает удалить исходный файл ключа: только в терминале, по умолчанию — нет
/// (App Store Connect даёт скачать ключ только один раз, и файл может быть единственной
/// резервной копией). Без терминала (CI, скрипты) вопроса нет и файл не трогается.
pub fn offer_to_delete_key_file(ui: &mut dyn Ui, key_file: &Path) {
    if !ui.is_interactive() {
        return;
    }
    let question = format!(
        "Delete the key file {}? App Store Connect lets you download a key only once, so \
         keep a backup (for example, in a password manager) before deleting it.",
        key_file.display()
    );
    match ui.confirm(&question, false) {
        Ok(true) => match fs::remove_file(key_file) {
            Ok(()) => ui.say(&format!("Deleted {}.", key_file.display())),
            Err(err) => ui.say(&format!(
                "Could not delete {}: {err}. Delete it yourself.",
                key_file.display()
            )),
        },
        Ok(false) => ui.say(&format!(
            "Kept {}. Store it safely or delete it: the server only needs the keychain copy.",
            key_file.display()
        )),
        // Ошибка ввода здесь не повод считать команду неудачной: ключ уже сохранён.
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_answers() {
        assert_eq!(parse_path_answer("  "), None);
        assert_eq!(
            parse_path_answer("'/tmp/AuthKey_X.p8' "),
            Some(PathBuf::from("/tmp/AuthKey_X.p8"))
        );
        assert_eq!(
            parse_path_answer("\"/tmp/a b.p8\""),
            Some(PathBuf::from("/tmp/a b.p8"))
        );
        assert_eq!(
            parse_path_answer("/tmp/a\\ b.p8"),
            Some(PathBuf::from("/tmp/a b.p8"))
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(parse_path_answer("~/k.p8"), Some(home.join("k.p8")));
        }
    }

    #[test]
    fn validates_flags() {
        KeyInput::default().validate().unwrap();
        let bad_id = KeyInput {
            key_id: Some("bad id"),
            ..KeyInput::default()
        };
        assert!(bad_id.validate().is_err());
        let bad_issuer = KeyInput {
            issuer_id: Some("nope"),
            ..KeyInput::default()
        };
        assert!(bad_issuer.validate().is_err());
    }
}
