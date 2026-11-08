//! Приём нового ключа API: общая часть `auth login` и `connect`.
//!
//! Ключ `.p8` принимается только путём к файлу (флагом или ответом на вопрос), не
//! содержимым в аргументе или переменной окружения. Он проверяется (PKCS#8, EC P-256),
//! по умолчанию проверяется запросом `GET /v1/apps?limit=1` и сохраняется только в
//! системном хранилище.
//!
//! В терминале без `--key-file` команда ведёт пользователя по шагам ([`store_new_key`]):
//! объясняет, что такое ключ, Key ID и Issuer ID, открывает страницу ключей App Store
//! Connect, ищет скачанный `AuthKey_*.p8` в каталоге загрузок и переспрашивает при
//! ошибочном ответе. Без терминала и с `--no-input` вопросов, браузера и пояснений нет:
//! всё берётся из флагов и профиля ([`require_complete_input`], [`crate::ui::SilentUi`]).
//!
//! Удалить исходный файл предлагает вызывающая команда ([`offer_to_delete_key_file`]),
//! один раз и в самом конце: так вопрос не повторяется, если команда делает несколько шагов.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, anyhow, bail};
use appstore_connect_api::auth::{KEYRING_SERVICE, KeyStore};
use appstore_connect_api::key::MAX_KEY_FILE_BYTES;
use appstore_connect_api::{ApiKey, Issuer, IssuerId, KeyId, PrivateKeyPem};

use crate::browser::{BrowserDecision, BrowserEnv, SkipReason, decide};
use crate::profiles::{ProfileSettings, validate_vendor_number};
use crate::ui::Ui;
use crate::verify::{CHECK_REQUEST, KeyVerifier};

/// Страница ключей App Store Connect API (Users and Access → Integrations).
pub const API_KEYS_URL: &str = "https://appstoreconnect.apple.com/access/integrations/api";

/// Раздел Payments and Financial Reports: там указан vendor number.
pub const PAYMENTS_URL: &str =
    "https://appstoreconnect.apple.com/itc/payments_and_financial_reports";

/// Сколько раз спрашивать в терминале, если ответ не подходит.
const ATTEMPTS: u32 = 3;

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

    /// Известен ли тип ключа без вопроса: из флагов или профиля.
    fn issuer_known(&self, settings: &ProfileSettings) -> bool {
        self.individual || self.issuer_id.is_some() || matches!(settings.issuer(), Ok(Some(_)))
    }
}

/// Чем помогать пользователю в терминале: браузер и каталог загрузок.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guide {
    /// Открывать ли ссылки в браузере.
    pub browser: BrowserDecision,
    /// Где искать скачанный `AuthKey_*.p8`; `None` — не искать.
    pub downloads: Option<PathBuf>,
}

impl Guide {
    /// По флагу `--no-browser` и окружению текущего процесса.
    pub fn detect(no_browser: bool) -> Self {
        Self {
            browser: decide(no_browser, &BrowserEnv::current()),
            downloads: dirs::download_dir(),
        }
    }

    /// Без браузера и поиска файлов.
    pub fn none() -> Self {
        Self {
            browser: BrowserDecision::Skip(SkipReason::Flag),
            downloads: None,
        }
    }
}

/// Новый ключ, сохранённый в системном хранилище.
#[derive(Debug)]
pub struct NewKey {
    pub key: ApiKey,
    /// Исходный файл ключа: его можно предложить удалить.
    pub file: PathBuf,
    /// Ключ получен пошаговым мастером: команда может предложить следующие шаги.
    pub guided: bool,
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

/// Без терминала и с `--no-input` вопросов нет: всё, что иначе спросили бы, должно прийти
/// флагами или из профиля. Ошибка перечисляет недостающие флаги разом.
pub fn require_complete_input(
    input: &KeyInput<'_>,
    settings: &ProfileSettings,
) -> anyhow::Result<()> {
    let mut missing = Vec::new();
    match input.key_file {
        None => missing.push("--key-file <AuthKey_XXXXXXXXXX.p8>"),
        Some(file) => {
            if input.key_id.is_none() && KeyId::from_file_name(file).is_none() {
                missing.push("--key-id <ID>");
            }
        }
    }
    if !input.issuer_known(settings) {
        missing.push("--issuer-id <UUID> (or --individual)");
    }
    if missing.is_empty() {
        return Ok(());
    }
    bail!(
        "missing {}: nothing is asked without a terminal or with --no-input. Create a key at \
         {API_KEYS_URL}, or run in a terminal without --no-input for a guided setup",
        missing.join(", ")
    )
}

/// Показывает ссылку и, если можно, открывает её в браузере.
pub fn show_link(ui: &mut dyn Ui, browser: BrowserDecision, url: &str) {
    ui.say(&format!("  {url}"));
    match browser {
        BrowserDecision::Open => match ui.open_url(url) {
            Ok(()) => ui.say("Opened the page in your browser."),
            Err(err) => ui.say(&format!(
                "Could not open a browser ({err}); open the link above yourself."
            )),
        },
        BrowserDecision::Skip(SkipReason::Ssh) => {
            ui.say("SSH session: open the link above in a browser on your computer.");
        }
        BrowserDecision::Skip(SkipReason::NoDisplay) => {
            ui.say("No graphical session: open the link above in a browser.");
        }
        BrowserDecision::Skip(SkipReason::Flag) => {}
    }
}

/// Спрашивает, пока ответ не подойдёт: в терминале до [`ATTEMPTS`] раз, без терминала —
/// один раз. `flag_hint` подсказывает флаг, если ответить не удалось.
fn ask_until<T>(
    ui: &mut dyn Ui,
    prompt: &str,
    flag_hint: &str,
    mut parse: impl FnMut(&str) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let attempts = if ui.is_interactive() { ATTEMPTS } else { 1 };
    let mut attempt = 1;
    loop {
        let answer = ui
            .ask(prompt)
            .with_context(|| format!("failed to read the answer; {flag_hint}"))?;
        match parse(&answer) {
            Ok(value) => return Ok(value),
            Err(err) if attempt < attempts => {
                ui.say(&format!("That did not work: {err:#}. Try again."));
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

fn step(ui: &mut dyn Ui, title: &str) {
    ui.say("");
    ui.say(title);
}

/// Шаг 1: что понадобится и зачем.
fn explain_key(ui: &mut dyn Ui, profile: &str) {
    step(ui, "Step 1 of 4. What you need");
    for line in [
        "appstore-connect-mcp works with App Store Connect on your behalf through an App Store \
         Connect API key. You need three things from App Store Connect:",
        "  - the key file AuthKey_<KEYID>.p8: a private key the server signs short-lived access \
         tokens with. Apple lets you download it only once;",
        "  - the Key ID: a 10-character code that names the key (it is also in the file name);",
        "  - the Issuer ID: a UUID that names your team; it is the same for all team keys.",
    ] {
        ui.say(line);
    }
    ui.say(&format!(
        "The key is stored only in the system keychain under profile `{profile}`, never in \
         config files."
    ));
    ui.say(
        "Who can create a key: the Account Holder or a user with the Admin role. If you are \
         neither, ask an Admin to create a team key and send you the .p8 file, the Key ID and \
         the Issuer ID over a secure channel, then continue from step 3.",
    );
}

/// Шаг 2: создание командного ключа на странице ключей.
fn show_create_steps(ui: &mut dyn Ui, guide: &Guide, profile: &str) {
    step(ui, "Step 2 of 4. Create a team key");
    ui.say("Open the App Store Connect API keys page (Users and Access > Integrations > App Store Connect API):");
    show_link(ui, guide.browser, API_KEYS_URL);
    for line in [
        "  1. Sign in with the Apple Account of your developer team.",
        "  2. If the page shows Request Access, the Account Holder has to click it and accept \
         the terms (once per team).",
        "  3. On the Team Keys tab, click Generate API Key (or +).",
    ] {
        ui.say(line);
    }
    ui.say(&format!(
        "  4. Name: anything that tells where the key is used, for example \
         \"appstore-connect-mcp {profile}\". Access: Admin (recommended: with narrower roles \
         some tools fail with HTTP 403)."
    ));
    ui.say(
        "  5. Click Generate, then Download in the row of the new key. Apple does not let you \
         download it again: keep a backup, for example in a password manager.",
    );
    ui.say(
        "An individual key also works (your user profile in App Store Connect > Individual \
         API Key > Generate), but it cannot manage certificates and profiles or download sales \
         and finance reports.",
    );
}

/// Скачанный файл ключа в каталоге загрузок.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedKey {
    pub path: PathBuf,
    pub modified: SystemTime,
}

/// Файлы `AuthKey_*.p8` в каталоге, новые первыми. Ошибки чтения каталога — пустой список.
pub fn downloaded_keys(dir: &Path) -> Vec<DownloadedKey> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut keys: Vec<DownloadedKey> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("AuthKey_") && name.ends_with(".p8")
        })
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| DownloadedKey {
                path: entry.path(),
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            })
        })
        .collect();
    keys.sort_by_key(|key| std::cmp::Reverse(key.modified));
    keys
}

/// Сколько прошло с момента сохранения файла, по-человечески.
fn age(since: SystemTime, now: SystemTime) -> String {
    let secs = now
        .duration_since(since)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    match secs {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// Путь для показа: домашний каталог — как `~`.
fn short_path(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Шаг 3: файл ключа. Сначала ищет скачанный файл, потом спрашивает путь.
fn choose_key_file(ui: &mut dyn Ui, guide: &Guide) -> anyhow::Result<(PathBuf, PrivateKeyPem)> {
    step(ui, "Step 3 of 4. The key file");
    if let Some(dir) = &guide.downloads {
        ui.ask("Press Enter when the .p8 file is downloaded: ")
            .context("failed to read the answer; pass --key-file <PATH>")?;
        let found = downloaded_keys(dir);
        match found.first() {
            Some(newest) => {
                if found.len() > 1 {
                    ui.say(&format!(
                        "Found {} key files in {}; the newest one is suggested.",
                        found.len(),
                        short_path(dir)
                    ));
                }
                let question = format!(
                    "Use {} (saved {})?",
                    short_path(&newest.path),
                    age(newest.modified, SystemTime::now())
                );
                if ui.confirm(&question, true).unwrap_or(false) {
                    match read_key_file(&newest.path) {
                        Ok(pem) => return Ok((newest.path.clone(), pem)),
                        Err(err) => ui.say(&format!("That did not work: {err:#}.")),
                    }
                }
            }
            None => ui.say(&format!("No AuthKey_*.p8 file in {}.", short_path(dir))),
        }
    }
    ask_until(
        ui,
        "Path to the key file (you can drag the file into this window): ",
        "pass --key-file <PATH>",
        |answer| {
            let path = parse_path_answer(answer).ok_or_else(|| anyhow!("no path given"))?;
            let pem = read_key_file(&path)?;
            Ok((path, pem))
        },
    )
}

/// Файл ключа без мастера: флаг, иначе вопрос.
fn resolve_key_file(
    ui: &mut dyn Ui,
    input: &KeyInput<'_>,
) -> anyhow::Result<(PathBuf, PrivateKeyPem)> {
    if let Some(path) = input.key_file {
        let pem = read_key_file(path)?;
        return Ok((path.to_path_buf(), pem));
    }
    ask_until(
        ui,
        "Path to the API key file (AuthKey_XXXXXXXXXX.p8): ",
        "pass --key-file <PATH>",
        |answer| {
            let path = parse_path_answer(answer)
                .ok_or_else(|| anyhow!("no key file given; pass --key-file <PATH>"))?;
            let pem = read_key_file(&path)?;
            Ok((path, pem))
        },
    )
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
    ask_until(
        ui,
        "Key ID (the KEY ID column next to the key in App Store Connect): ",
        "pass --key-id <ID>",
        |answer| Ok(KeyId::new(answer)?),
    )
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
    ask_issuer(ui)
}

fn ask_issuer(ui: &mut dyn Ui) -> anyhow::Result<Issuer> {
    let issuer = ask_until(
        ui,
        "Issuer ID (leave empty for an individual key): ",
        "pass --issuer-id <UUID> or --individual",
        |answer| {
            let answer = answer.trim();
            if answer.is_empty() {
                return Ok(Issuer::Individual);
            }
            IssuerId::new(answer).map(Issuer::Team).map_err(|_| {
                anyhow!(
                    "`{answer}` is not an Issuer ID: expected a UUID like \
                     01234567-89ab-cdef-0123-456789abcdef"
                )
            })
        },
    )?;
    if issuer == Issuer::Individual {
        ui.say("Using an individual key (no Issuer ID).");
    }
    Ok(issuer)
}

/// Где взять Issuer ID; страница ключей открывается, если её ещё не показывали.
fn explain_issuer(ui: &mut dyn Ui, guide: &Guide, wizard: bool, page_shown: bool) {
    if wizard {
        step(ui, "Step 4 of 4. Issuer ID");
    }
    ui.say(
        "The Issuer ID is on the App Store Connect API keys page, above the list of team keys, \
         with a Copy button next to it. Paste it here.",
    );
    if !page_shown {
        show_link(ui, guide.browser, API_KEYS_URL);
    }
}

/// Читает новый ключ из файла, проверяет его (если `verifier` задан) и сохраняет в системном
/// хранилище. Если проверка не прошла, ничего не сохраняется.
///
/// В терминале без `--key-file` ведёт по шагам: что такое ключ, как его создать, где файл
/// и Issuer ID. Ошибочные ответы переспрашиваются, а если ключ не прошёл проверку, можно
/// ещё раз ввести Key ID и Issuer ID.
pub async fn store_new_key<V: KeyVerifier>(
    store: &dyn KeyStore,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    guide: &Guide,
    profile: &str,
    input: &KeyInput<'_>,
    settings: &ProfileSettings,
) -> anyhow::Result<NewKey> {
    if !ui.is_interactive() {
        require_complete_input(input, settings)?;
    }
    let wizard = ui.is_interactive() && input.key_file.is_none();
    let (file, pem) = if wizard {
        explain_key(ui, profile);
        show_create_steps(ui, guide, profile);
        choose_key_file(ui, guide)?
    } else {
        resolve_key_file(ui, input)?
    };
    let mut key_id = resolve_new_key_id(ui, input, &file)?;
    if ui.is_interactive() && !input.issuer_known(settings) {
        explain_issuer(ui, guide, wizard, wizard);
    }
    let mut issuer = resolve_issuer(ui, input, settings)?;

    let mut attempt = 1;
    let key = loop {
        let key = ApiKey::new(&pem, key_id.clone(), issuer.clone())?;
        let Some(verifier) = verifier else {
            ui.say("Skipping the key check (--no-verify).");
            break key;
        };
        ui.say(&format!("Checking the key with {CHECK_REQUEST}..."));
        match verifier.check(&key).await {
            Ok(outcome) => {
                ui.say(&format!("Key is valid: {outcome}."));
                break key;
            }
            Err(err) => {
                if ui.is_interactive() && attempt < ATTEMPTS {
                    ui.say(&format!("The key check failed: {err:#}"));
                    if ui
                        .confirm("Enter the Key ID and the Issuer ID again?", true)
                        .unwrap_or(false)
                    {
                        key_id = ask_key_id(ui)?;
                        issuer = ask_issuer(ui)?;
                        attempt += 1;
                        continue;
                    }
                }
                bail!(
                    "{err:#} Nothing was stored. Re-run with --no-verify to store the key \
                     without verification."
                );
            }
        }
    };

    let replaced = store.contains(profile)?;
    store.set(profile, &pem)?;
    ui.say(&format!(
        "API key {} in the system keychain (service `{KEYRING_SERVICE}`, account `{profile}`).",
        if replaced { "replaced" } else { "saved" }
    ));
    Ok(NewKey {
        key,
        file,
        guided: wizard,
    })
}

/// Записывает Key ID и тип ключа в настройки профиля.
pub fn remember_key(settings: &mut ProfileSettings, key: &ApiKey) {
    settings.key_id = Some(key.key_id().as_str().to_owned());
    settings.set_issuer(key.issuer());
}

/// Необязательный шаг мастера: vendor number для отчётов продаж и финансов. Только в
/// терминале, если номера ещё нет и ключ командный (индивидуальному отчёты недоступны).
pub fn offer_vendor_number(ui: &mut dyn Ui, guide: &Guide, settings: &mut ProfileSettings) {
    let individual = matches!(settings.issuer(), Ok(Some(Issuer::Individual)));
    if !ui.is_interactive() || settings.vendor_number.is_some() || individual {
        return;
    }
    step(ui, "Optional: sales and finance reports");
    ui.say(
        "Sales and finance reports are requested by vendor number. Skip this if you do not \
         need them; you can add it later with `appstore-connect-mcp auth login --vendor-number \
         <NUMBER>`.",
    );
    if !ui
        .confirm("Set up the vendor number now?", false)
        .unwrap_or(false)
    {
        return;
    }
    ui.say(
        "The vendor number is at the top left of Payments and Financial Reports, under your \
         legal entity name (needs the Account Holder, Admin or Finance role):",
    );
    show_link(ui, guide.browser, PAYMENTS_URL);
    ui.say(
        "If the link does not open the page, open the App Store Connect home page and click \
         Payments and Financial Reports.",
    );
    let answer = ask_until(
        ui,
        "Vendor number (leave empty to skip): ",
        "pass --vendor-number <NUMBER>",
        |answer| {
            if answer.trim().is_empty() {
                Ok(None)
            } else {
                validate_vendor_number(answer).map(Some)
            }
        },
    );
    match answer {
        Ok(Some(number)) => {
            ui.say(&format!("Vendor number: {number}."));
            settings.vendor_number = Some(number);
        }
        Ok(None) => ui.say("Skipped the vendor number."),
        Err(err) => ui.say(&format!("Skipped the vendor number: {err:#}")),
    }
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

    #[test]
    fn silent_mode_lists_all_missing_flags() {
        let settings = ProfileSettings::default();
        let err = require_complete_input(&KeyInput::default(), &settings)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--key-file"), "{err}");
        assert!(
            err.contains("--issuer-id <UUID> (or --individual)"),
            "{err}"
        );
        assert!(err.contains(API_KEYS_URL), "{err}");

        // Key ID не выводится из имени файла — нужен флаг.
        let file = Path::new("/tmp/key.p8");
        let input = KeyInput {
            key_file: Some(file),
            individual: true,
            ..KeyInput::default()
        };
        let err = require_complete_input(&input, &settings)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--key-id"), "{err}");
        assert!(!err.contains("--issuer-id"), "{err}");

        // Всё есть: Key ID из имени файла, Issuer ID из профиля.
        let file = Path::new("/tmp/AuthKey_TESTKEY123.p8");
        let input = KeyInput {
            key_file: Some(file),
            ..KeyInput::default()
        };
        let settings = ProfileSettings {
            issuer_id: Some("01234567-89ab-cdef-0123-456789abcdef".into()),
            ..ProfileSettings::default()
        };
        require_complete_input(&input, &settings).unwrap();
    }

    #[test]
    fn finds_downloaded_keys_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        assert!(downloaded_keys(dir.path()).is_empty());
        let old = dir.path().join("AuthKey_OLDKEY1234.p8");
        let new = dir.path().join("AuthKey_NEWKEY1234.p8");
        fs::write(&old, "x").unwrap();
        fs::write(&new, "x").unwrap();
        fs::write(dir.path().join("notes.p8"), "x").unwrap();
        fs::write(dir.path().join("AuthKey_X.txt"), "x").unwrap();
        fs::create_dir(dir.path().join("AuthKey_DIR.p8")).unwrap();
        let past = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();
        let found: Vec<PathBuf> = downloaded_keys(dir.path())
            .into_iter()
            .map(|key| key.path)
            .collect();
        assert_eq!(found, [new, old]);
        assert!(downloaded_keys(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn short_paths() {
        assert_eq!(
            short_path(Path::new("/tmp/AuthKey_X.p8")),
            "/tmp/AuthKey_X.p8"
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                short_path(&home.join("Downloads").join("AuthKey_X.p8")),
                "~/Downloads/AuthKey_X.p8"
            );
        }
    }

    #[test]
    fn ages() {
        let now = SystemTime::now();
        assert_eq!(age(now, now), "just now");
        assert_eq!(age(now - Duration::from_secs(150), now), "2 min ago");
        assert_eq!(age(now - Duration::from_secs(7200), now), "2 h ago");
        assert_eq!(
            age(now - Duration::from_secs(3 * 86_400), now),
            "3 days ago"
        );
        // Время файла в будущем — не паника.
        assert_eq!(age(now + Duration::from_secs(60), now), "just now");
    }
}
