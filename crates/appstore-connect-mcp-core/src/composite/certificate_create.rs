//! Составной инструмент `certificate_create`: сертификат подписи без Keychain Access.
//!
//! Ключ RSA 2048 и CSR (PKCS#10) создаются локально, App Store Connect выпускает по CSR
//! сертификат (`POST /v1/certificates`). Приватный ключ сохраняется в хранилище секретов
//! всегда — иначе его не восстановить, а сертификат без ключа бесполезен. По `save_to`
//! собирается `.p12` (сертификат и ключ) с паролем из файла или со случайным паролем,
//! который тоже кладётся в хранилище.
//!
//! Ключ, пароль, CSR и содержимое сертификата в ответ, ошибки и логи не попадают: агент
//! получает только идентификаторы, имена записей хранилища и путь к файлу.

use std::{path::PathBuf, str::FromStr};

use appstore_connect_api::{
    AccessLevel, HttpMethod,
    executor::{ApiRequest, ResponseBody},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use der::{Decode, Encode, EncodePem};
use p12_keystore::{
    Certificate as P12Certificate, EncryptionAlgorithm, KeyStore, KeyStoreEntry, MacAlgorithm,
    PrivateKey as P12PrivateKey, PrivateKeyChain,
};
use pkcs8::{EncodePrivateKey, LineEnding};
use rand::RngExt;
use rsa::{
    pkcs1v15::{Signature, SigningKey},
    sha2::Sha256,
    signature::RandomizedSigner,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use spki::{DynSignatureAlgorithmIdentifier, SignatureBitStringEncoding};
use x509_cert::{
    Certificate,
    name::Name,
    request::{CertReq, CertReqInfo, Version},
    spki::SubjectPublicKeyInfoOwned,
};
use zeroize::Zeroizing;

use super::{CompositeTool, Context, ToolError, ToolFuture, ToolResult};

/// Apple выпускает сертификаты только по ключам RSA 2048.
const KEY_BITS: usize = 2048;
/// CN в CSR по умолчанию. Apple подставляет в сертификат свои имя и команду, CN из CSR
/// виден только в списке ключей.
const DEFAULT_COMMON_NAME: &str = "appstore-connect-mcp";
/// Верхняя граница `ub-common-name` из X.520.
const MAX_COMMON_NAME: usize = 64;
/// Длина случайного пароля `.p12` (буквы и цифры, ~190 бит).
const PASSWORD_LEN: usize = 32;
/// Итерации KDF и MAC `.p12`.
const P12_ITERATIONS: u32 = 10_000;
/// `operationId` создания сертификата: из его схемы берётся перечисление типов.
const CREATE_OPERATION_ID: &str = "certificates_createInstance";
/// Путь к перечислению `certificateType` в JSON Schema аргументов операции.
const CERTIFICATE_TYPE_POINTER: &str =
    "/properties/body/properties/data/properties/attributes/properties/certificateType/enum";

const DESCRIPTION: &str = "Create a signing certificate without Keychain Access: generates an \
RSA 2048 private key and a certificate signing request locally, asks App Store Connect to issue \
the certificate (`POST /v1/certificates`) and stores the private key in the server's secret \
vault (entry `certificate/<id>/private-key`), so it is never lost. With `save_to` it also writes \
a new password-protected `.p12` file (certificate + private key, 3DES/SHA-1 PKCS#12 that macOS \
`security import`, Keychain Access, Xcode and OpenSSL accept). The `.p12` password comes from \
`p12_password_file` (a local file prepared by the user) or is generated and stored in the vault \
(entry `certificate/<id>/p12-password`). Returns the certificate id, type, name, serial number, \
expiration date, the vault entry names and the saved path; the private key, the password, the \
CSR and the certificate content are never returned. Apple limits the number of certificates per \
type: check `list_certificates` first and do not call this tool again if a call reported that \
the certificate was created. Pass type ID and merchant ID certificates need `pass_type_id` or \
`merchant_id`.";

pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: "certificate_create",
        toolset: "provisioning",
        access: AccessLevel::Write,
        description: DESCRIPTION,
        input_schema,
        uses_local_files: true,
        run,
    }]
}

fn run<'a>(ctx: &'a Context, args: Value) -> ToolFuture<'a> {
    Box::pin(create(ctx, args))
}

fn input_schema() -> Value {
    let mut certificate_type = json!({
        "type": "string",
        "description": "Certificate type, for example IOS_DISTRIBUTION, DISTRIBUTION (Apple \
            Distribution), DEVELOPMENT (Apple Development), DEVELOPER_ID_APPLICATION."
    });
    if let (Some(types), Some(obj)) = (certificate_types(), certificate_type.as_object_mut()) {
        obj.insert("enum".into(), Value::from(types));
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["certificate_type"],
        "properties": {
            "certificate_type": certificate_type,
            "common_name": {
                "type": "string",
                "maxLength": MAX_COMMON_NAME,
                "description": "Common name for the certificate signing request (letters, \
                    digits, spaces and . _ - ( ) ' @ &). Default: appstore-connect-mcp."
            },
            "save_to": {
                "type": "string",
                "description": "Path of a new .p12 file with the certificate and its private \
                    key. Existing files are not overwritten."
            },
            "p12_password_file": {
                "type": "string",
                "description": "Local file with the .p12 password (the first line is used). \
                    Requires save_to. Without it a random password is generated and stored in \
                    the vault. Never put the password itself into arguments."
            },
            "pass_type_id": {
                "type": "string",
                "description": "Pass type ID resource id, for PASS_TYPE_ID and \
                    PASS_TYPE_ID_WITH_NFC certificates."
            },
            "merchant_id": {
                "type": "string",
                "description": "Merchant ID resource id, for Apple Pay certificates."
            }
        }
    })
}

/// Типы сертификатов из спецификации Apple (перечисление `CertificateType` в теле
/// `POST /v1/certificates`), чтобы не дублировать его вручную.
fn certificate_types() -> Option<Vec<String>> {
    let op = appstore_connect_api::spec::operations()
        .iter()
        .find(|op| op.operation_id == CREATE_OPERATION_ID)?;
    let schema: Value = serde_json::from_str(op.input_schema).ok()?;
    let types: Vec<String> = schema
        .pointer(CERTIFICATE_TYPE_POINTER)?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    (!types.is_empty()).then_some(types)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    certificate_type: String,
    common_name: Option<String>,
    save_to: Option<String>,
    p12_password_file: Option<String>,
    pass_type_id: Option<String>,
    merchant_id: Option<String>,
}

/// Имя записи хранилища с приватным ключом сертификата.
fn private_key_account(certificate_id: &str) -> String {
    format!("certificate/{certificate_id}/private-key")
}

/// Имя записи хранилища с паролем `.p12`.
fn p12_password_account(certificate_id: &str) -> String {
    format!("certificate/{certificate_id}/p12-password")
}

/// Куда и с каким паролем писать `.p12`.
struct P12Plan {
    path: PathBuf,
    /// Пароль из `p12_password_file`; `None` — сгенерировать и сохранить в хранилище.
    password: Option<Zeroizing<String>>,
}

/// Ключ и CSR, созданные локально.
struct LocalKey {
    key_pem: Zeroizing<String>,
    key_der: Zeroizing<Vec<u8>>,
    spki_der: Vec<u8>,
    csr_pem: String,
}

/// Данные выпущенного сертификата из ответа Apple.
struct Issued {
    id: String,
    attributes: Map<String, Value>,
}

async fn create(ctx: &Context, args: Value) -> ToolResult {
    let args: Args = serde_json::from_value(args)
        .map_err(|e| ToolError::new(format!("invalid arguments: {e}")))?;
    validate_certificate_type(&args.certificate_type)?;
    let common_name = match args.common_name.as_deref() {
        Some(name) => validate_common_name(name)?,
        None => DEFAULT_COMMON_NAME.to_string(),
    };
    for (name, value) in [
        ("pass_type_id", &args.pass_type_id),
        ("merchant_id", &args.merchant_id),
    ] {
        if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
            return Err(ToolError::new(format!("`{name}` must not be empty")));
        }
    }

    // Всё, что может не получиться локально, проверяется до запроса к Apple: после выпуска
    // сертификата отменить его можно только отзывом.
    let plan = match (&args.save_to, &args.p12_password_file) {
        (None, Some(_)) => {
            return Err(ToolError::new(
                "`p12_password_file` requires `save_to` (the path of the .p12 file)",
            ));
        }
        (None, None) => None,
        (Some(save_to), password_file) => {
            let path = ctx.files.resolve_write(save_to)?;
            let password = match password_file {
                Some(file) => Some(read_password(ctx, file).await?),
                None => None,
            };
            Some(P12Plan { path, password })
        }
    };
    // Хранилище недоступно (например, заблокирован Keychain) — узнаём это до выпуска
    // сертификата, а не когда ключ уже некуда положить.
    let probe = private_key_account("probe");
    ctx.vault.contains(&probe).map_err(|e| {
        ToolError::new(format!(
            "the secret vault is not available, so the private key could not be kept; nothing \
             was created: {e}"
        ))
    })?;

    let local = generate_key_and_csr(common_name).await?;

    let mut data = json!({
        "type": "certificates",
        "attributes": {
            // Apple принимает CSR в PEM, как его выдаёт Keychain Access.
            "csrContent": local.csr_pem,
            "certificateType": args.certificate_type,
        }
    });
    let mut relationships = Map::new();
    if let Some(id) = &args.pass_type_id {
        relationships.insert(
            "passTypeId".into(),
            json!({"data": {"type": "passTypeIds", "id": id}}),
        );
    }
    if let Some(id) = &args.merchant_id {
        relationships.insert(
            "merchantId".into(),
            json!({"data": {"type": "merchantIds", "id": id}}),
        );
    }
    if !relationships.is_empty() {
        data["relationships"] = Value::Object(relationships);
    }
    let request =
        ApiRequest::new(HttpMethod::Post, "/v1/certificates").with_json(json!({ "data": data }));
    // Ошибка Apple: сертификата нет, в хранилище ничего не записано.
    let response = ctx.api.execute(request).await?;
    let issued = parse_issued(response.body)?;

    let key_account = private_key_account(&issued.id);
    let key_stored = ctx.vault.store(&key_account, &local.key_pem);

    let p12_outcome = match plan {
        Some(plan) => Some(export_p12(ctx, &issued, &local, plan).await),
        None => None,
    };

    let mut out = summary(&issued);
    match (key_stored, p12_outcome) {
        (Ok(()), None) => {
            out.insert("private_key_vault_entry".into(), json!(key_account));
        }
        (Ok(()), Some(Ok(saved))) => {
            out.insert("private_key_vault_entry".into(), json!(key_account));
            saved.write_into(&mut out);
        }
        (Ok(()), Some(Err(e))) => {
            return Err(ToolError::new(format!(
                "certificate {id} was created and its private key is stored in the secret \
                 vault (entry `{key_account}`), but the .p12 file was not written: {e}. The \
                 certificate and its key are kept, so the .p12 can be exported from the vault \
                 entry later: do not create another certificate.",
                id = issued.id
            )));
        }
        (Err(vault), Some(Ok(saved))) => {
            return Err(ToolError::new(format!(
                "certificate {id} was created and the .p12 file with its private key was \
                 written to {path}, but the key could not be stored in the secret vault: \
                 {vault}. Keep the .p12 file: it is the only copy of the private key. Do not \
                 create another certificate.",
                id = issued.id,
                path = saved.path.display()
            )));
        }
        (Err(vault), p12) => {
            let p12_note = match p12 {
                Some(Err(e)) => format!(" The .p12 file was not written either: {e}."),
                _ => String::new(),
            };
            return Err(ToolError::new(format!(
                "certificate {id} was created, but its private key could not be stored in the \
                 secret vault: {vault}.{p12_note} The private key is lost: revoke certificate \
                 {id} and create a new one once the vault is available.",
                id = issued.id
            )));
        }
    }
    Ok(Value::Object(out))
}

fn validate_certificate_type(value: &str) -> Result<(), ToolError> {
    match certificate_types() {
        Some(types) if !types.iter().any(|t| t == value) => Err(ToolError::new(format!(
            "unknown certificate_type `{value}`; expected one of: {}",
            types.join(", ")
        ))),
        _ if value.is_empty() => Err(ToolError::new("`certificate_type` must not be empty")),
        _ => Ok(()),
    }
}

/// CN без символов, которые в строковой записи имени (RFC 4514) требуют экранирования:
/// имя собирается из `CN=<значение>`, и лишний `,` или `+` добавил бы атрибуты.
fn validate_common_name(name: &str) -> Result<String, ToolError> {
    let allowed = |c: char| c.is_alphanumeric() || " ._-()'@&".contains(c);
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > MAX_COMMON_NAME
        || !trimmed.chars().all(allowed)
    {
        return Err(ToolError::new(format!(
            "invalid common_name: use 1-{MAX_COMMON_NAME} letters, digits, spaces and . _ - ( ) ' @ &"
        )));
    }
    Ok(trimmed.to_string())
}

/// Пароль `.p12` — первая строка файла без перевода строки. Значение в ошибки не попадает.
async fn read_password(ctx: &Context, file: &str) -> Result<Zeroizing<String>, ToolError> {
    let path = ctx.files.resolve_read(file)?;
    let raw = Zeroizing::new(tokio::fs::read(&path).await.map_err(|e| {
        ToolError::new(format!(
            "could not read p12_password_file {}: {}",
            path.display(),
            e.kind()
        ))
    })?);
    let text = std::str::from_utf8(&raw)
        .map_err(|_| ToolError::new("p12_password_file must contain UTF-8 text"))?;
    let line = text.split(['\n', '\r']).next().unwrap_or_default();
    if line.is_empty() {
        return Err(ToolError::new(
            "p12_password_file is empty: put the password on its first line",
        ));
    }
    Ok(Zeroizing::new(line.to_string()))
}

/// Генерация ключа и подпись CSR — счётные операции, выполняются вне потоков рантайма.
async fn generate_key_and_csr(common_name: String) -> Result<LocalKey, ToolError> {
    tokio::task::spawn_blocking(move || build_key_and_csr(&common_name))
        .await
        .map_err(|e| ToolError::new(format!("key generation task failed: {e}")))?
}

fn build_key_and_csr(common_name: &str) -> Result<LocalKey, ToolError> {
    let failed = |what: &str, e: &dyn std::fmt::Display| {
        ToolError::new(format!("could not create the {what}: {e}"))
    };
    let mut rng = rand::rng();
    let key = rsa::PrivateKey::new(&mut rng, KEY_BITS).map_err(|e| failed("RSA key", &e))?;
    let public_key = SubjectPublicKeyInfoOwned::from_key(&key.to_public_key())
        .map_err(|e| failed("public key info", &e))?;
    let spki_der = public_key
        .to_der()
        .map_err(|e| failed("public key info", &e))?;
    let key_pem = key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| failed("private key PEM", &e))?;
    let key_der = Zeroizing::new(
        key.to_pkcs8_der()
            .map_err(|e| failed("private key DER", &e))?
            .as_bytes()
            .to_vec(),
    );

    let subject = Name::from_str(&format!("CN={common_name}"))
        .map_err(|e| failed("certificate signing request subject", &e))?;
    // CSR без атрибутов (`[0]` пустой), как у Keychain Access: пустой extensionRequest
    // некоторые центры сертификации отклоняют.
    let info = CertReqInfo {
        version: Version::V1,
        subject,
        public_key,
        attributes: Default::default(),
    };
    let signer = SigningKey::<Sha256>::new(key);
    let info_der = info.to_der().map_err(|e| failed("CSR", &e))?;
    let signature: Signature = signer
        .try_sign_with_rng(&mut rng, &info_der)
        .map_err(|e| failed("CSR signature", &e))?;
    let csr = CertReq {
        info,
        algorithm: signer
            .signature_algorithm_identifier()
            .map_err(|e| failed("CSR", &e))?,
        signature: signature.to_bitstring().map_err(|e| failed("CSR", &e))?,
    };
    let csr_pem = csr.to_pem(LineEnding::LF).map_err(|e| failed("CSR", &e))?;
    Ok(LocalKey {
        key_pem,
        key_der,
        spki_der,
        csr_pem,
    })
}

fn parse_issued(body: ResponseBody) -> Result<Issued, ToolError> {
    let ResponseBody::Json(mut body) = body else {
        return Err(ToolError::new(
            "App Store Connect returned an unexpected response to the certificate request",
        ));
    };
    let data = body.get_mut("data").map(Value::take).unwrap_or(Value::Null);
    let id = data
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let attributes = match data.get("attributes") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    // Идентификатор входит в имя записи хранилища: только безопасные символы.
    let valid_id = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid_id {
        return Err(ToolError::new(
            "App Store Connect reported success but returned no usable certificate id; the \
             private key was not stored. Check list_certificates and revoke a certificate \
             created just now: it has no private key.",
        ));
    }
    Ok(Issued { id, attributes })
}

/// Сводка сертификата для ответа: без `certificateContent`.
fn summary(issued: &Issued) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("id".into(), json!(issued.id));
    for (api, ours) in [
        ("certificateType", "certificate_type"),
        ("name", "name"),
        ("displayName", "display_name"),
        ("serialNumber", "serial_number"),
        ("platform", "platform"),
        ("expirationDate", "expiration_date"),
    ] {
        if let Some(value) = issued.attributes.get(api).filter(|v| !v.is_null()) {
            out.insert(ours.into(), value.clone());
        }
    }
    out
}

/// Записанный `.p12`.
struct SavedP12 {
    path: PathBuf,
    bytes: usize,
    /// Запись хранилища со сгенерированным паролем; `None` — пароль из файла.
    password_entry: Option<String>,
}

impl SavedP12 {
    fn write_into(self, out: &mut Map<String, Value>) {
        out.insert("saved_to".into(), json!(self.path.display().to_string()));
        out.insert("bytes".into(), json!(self.bytes));
        match self.password_entry {
            Some(entry) => out.insert("p12_password_vault_entry".into(), json!(entry)),
            None => out.insert("p12_password".into(), json!("from p12_password_file")),
        };
    }
}

async fn export_p12(
    ctx: &Context,
    issued: &Issued,
    local: &LocalKey,
    plan: P12Plan,
) -> Result<SavedP12, ToolError> {
    let content = issued
        .attributes
        .get("certificateContent")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new("the response has no certificate content"))?;
    let cert_der = STANDARD
        .decode(content.trim())
        .map_err(|_| ToolError::new("the certificate content is not valid base64"))?;
    let cert = Certificate::from_der(&cert_der)
        .map_err(|e| ToolError::new(format!("the certificate content is not X.509 DER: {e}")))?;
    let cert_spki = cert
        .tbs_certificate()
        .subject_public_key_info()
        .to_der()
        .map_err(|e| ToolError::new(format!("could not read the certificate key: {e}")))?;
    if cert_spki != local.spki_der {
        return Err(ToolError::new(
            "the issued certificate does not match the generated key",
        ));
    }

    let (password, password_entry) = match plan.password {
        Some(password) => (password, None),
        None => {
            let password = generate_password();
            let entry = p12_password_account(&issued.id);
            // Пароль сохраняется до записи файла: файл без пароля бесполезен.
            ctx.vault.store(&entry, &password).map_err(|e| {
                ToolError::new(format!(
                    "the generated .p12 password could not be stored in the vault: {e}"
                ))
            })?;
            (password, Some(entry))
        }
    };

    let alias = issued
        .attributes
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty() && name.chars().all(|c| u32::from(c) <= 0xFFFF))
        .unwrap_or(&issued.id)
        .to_string();
    let key_der = local.key_der.clone();
    let local_key_id = issued.id.clone().into_bytes();
    let p12 = tokio::task::spawn_blocking(move || {
        build_p12(&alias, local_key_id, &key_der, &cert_der, &password)
    })
    .await
    .map_err(|e| ToolError::new(format!("the .p12 task failed: {e}")))??;

    write_new_file(&plan.path, &p12).await?;
    Ok(SavedP12 {
        path: plan.path,
        bytes: p12.len(),
        password_entry,
    })
}

/// Случайный пароль из букв и цифр: такой пароль без проблем вводится в Keychain Access
/// и передаётся в `security import -P`.
fn generate_password() -> Zeroizing<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::rng();
    Zeroizing::new(
        (0..PASSWORD_LEN)
            .map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
            .collect(),
    )
}

/// PKCS#12 с ключом и сертификатом. Шифрование — 3DES (PKCS#12 PBE с SHA-1), MAC — HMAC-SHA1:
/// этот формат экспортирует сам Keychain Access, его принимают `security import` всех
/// версий macOS, Xcode, fastlane и OpenSSL 3 без `-legacy`. AES-256/PBKDF2 (формат OpenSSL 3
/// по умолчанию) старые macOS не импортируют.
fn build_p12(
    alias: &str,
    local_key_id: Vec<u8>,
    key_der: &[u8],
    cert_der: &[u8],
    password: &str,
) -> Result<Vec<u8>, ToolError> {
    let failed = |e: p12_keystore::error::Error| {
        ToolError::new(format!("could not build the .p12 file: {e}"))
    };
    let key = P12PrivateKey::from_der(key_der).map_err(failed)?;
    let cert = P12Certificate::from_der(cert_der).map_err(failed)?;
    let mut store = KeyStore::new();
    store.add_entry(
        alias,
        KeyStoreEntry::PrivateKeyChain(PrivateKeyChain::new(local_key_id, key, [cert])),
    );
    store
        .writer(password)
        .encryption_algorithm(EncryptionAlgorithm::PbeWithShaAnd3KeyTripleDesCbc)
        .encryption_iterations(P12_ITERATIONS)
        .mac_algorithm(MacAlgorithm::HmacSha1)
        .mac_iterations(P12_ITERATIONS)
        .write()
        .map_err(failed)
}

/// Новый файл (существующий не перезаписывается, частично записанный удаляется); в Unix —
/// с правами 0600: в `.p12` лежит приватный ключ, пусть и зашифрованный.
pub(super) async fn write_new_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), ToolError> {
    use tokio::io::AsyncWriteExt;

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let fail =
        |e: std::io::Error| ToolError::new(format!("could not write {}: {e}", path.display()));
    let mut file = options.open(path).await.map_err(fail)?;
    let written = async {
        file.write_all(bytes).await?;
        file.sync_all().await
    }
    .await;
    if let Err(e) = written {
        drop(file);
        let _ = tokio::fs::remove_file(path).await;
        return Err(fail(e));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{sync::Arc, time::Duration};

    use appstore_connect_api::{
        ApiKey, Issuer, IssuerId, JwtTokens, KeyId, PrivateKeyPem,
        executor::{ApiClient, ClientConfig, RetryPolicy},
        external::ExternalClient,
    };
    use p12_keystore::Pkcs12ImportPolicy;
    use p256::elliptic_curve::Generate;
    use rsa::{pkcs1v15::VerifyingKey, signature::Verifier};
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{method, path},
    };
    use x509_cert::{
        builder::{Builder, CertificateBuilder, profile::BuilderProfile},
        der::DecodePem,
        ext::Extension,
        serial_number::SerialNumber,
        time::Validity,
    };

    use super::*;
    use crate::composite::{
        ProfileInfo, SecretVault,
        testing::{DirFiles, MemoryVault},
    };

    /// Хранилище, которое считает записи: ошибка Apple не должна записать ничего.
    #[derive(Default)]
    struct CountingVault {
        inner: MemoryVault,
        stores: std::sync::atomic::AtomicUsize,
    }

    impl SecretVault for CountingVault {
        fn store(&self, account: &str, value: &str) -> Result<(), ToolError> {
            self.stores
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.store(account, value)
        }

        fn contains(&self, account: &str) -> Result<bool, ToolError> {
            self.inner.contains(account)
        }
    }

    /// Контекст с клиентом API на `wiremock` (ключ ES256 сгенерирован в тесте).
    pub(crate) fn context(
        server: &MockServer,
        dir: &std::path::Path,
        vault: Arc<dyn SecretVault>,
        access: AccessLevel,
    ) -> Context {
        let signing = p256::ecdsa::SigningKey::generate();
        let pem = signing.to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap();
        let key = ApiKey::new(
            &PrivateKeyPem::parse(&pem).unwrap(),
            KeyId::new("TESTKEY123").unwrap(),
            Issuer::Team(IssuerId::new("01234567-89ab-cdef-0123-456789abcdef").unwrap()),
        )
        .unwrap();
        let config = ClientConfig {
            base_url: url::Url::parse(&server.uri()).unwrap(),
            tokens: Some(Arc::new(JwtTokens::new(key))),
            timeout: Duration::from_secs(10),
            max_response_bytes: 1024 * 1024,
            retry: RetryPolicy {
                max_retries: 0,
                default_delay: Duration::from_millis(10),
                max_delay: Duration::from_millis(10),
            },
        };
        Context {
            api: Arc::new(ApiClient::new(config).unwrap()),
            external: Arc::new(ExternalClient::new(Duration::from_secs(10), 1024 * 1024).unwrap()),
            files: Arc::new(DirFiles::new(dir)),
            vault,
            profile: ProfileInfo {
                name: "test".into(),
                vendor_number: None,
            },
            access,
        }
    }

    /// Профиль сертификата «от Apple»: издатель — тестовый CA, без расширений.
    struct FakeApple {
        issuer: Name,
        subject: Name,
    }

    impl BuilderProfile for FakeApple {
        fn get_issuer(&self, _subject: &Name) -> Name {
            self.issuer.clone()
        }

        fn get_subject(&self) -> Name {
            self.subject.clone()
        }

        fn build_extensions(
            &self,
            _spk: spki::SubjectPublicKeyInfoRef<'_>,
            _issuer_spk: spki::SubjectPublicKeyInfoRef<'_>,
            _tbs: &x509_cert::TbsCertificate,
        ) -> x509_cert::builder::Result<Vec<Extension>> {
            Ok(Vec::new())
        }
    }

    /// Выпускает сертификат по CSR так, как это делает Apple: на ключ из CSR, подпись CA.
    fn issue(csr_pem: &str, ca: &SigningKey<Sha256>) -> Vec<u8> {
        let csr = CertReq::from_pem(csr_pem).unwrap();
        let builder = CertificateBuilder::new(
            FakeApple {
                issuer: Name::from_str("CN=Test WWDR CA").unwrap(),
                subject: Name::from_str("CN=iPhone Distribution: Test Team").unwrap(),
            },
            SerialNumber::from(0x1234_5678u32),
            Validity::from_now(Duration::from_secs(3600)).unwrap(),
            csr.info.public_key.clone(),
        )
        .unwrap();
        builder
            .build_with_rng::<_, Signature, _>(ca, &mut rand::rng())
            .unwrap()
            .to_der()
            .unwrap()
    }

    /// Мок `POST /v1/certificates`: проверяет CSR, выпускает сертификат, запоминает CSR.
    async fn mock_apple(server: &MockServer, csr_seen: Arc<std::sync::Mutex<String>>) {
        let ca = SigningKey::<Sha256>::new(rsa::PrivateKey::new(&mut rand::rng(), 2048).unwrap());
        Mock::given(method("POST"))
            .and(path("/v1/certificates"))
            .respond_with(move |req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let attrs = &body["data"]["attributes"];
                assert_eq!(attrs["certificateType"], "IOS_DISTRIBUTION");
                let csr = attrs["csrContent"].as_str().unwrap().to_string();
                let cert = issue(&csr, &ca);
                *csr_seen.lock().unwrap() = csr;
                ResponseTemplate::new(201).set_body_json(json!({
                    "data": {
                        "type": "certificates",
                        "id": "CERT123ABC",
                        "attributes": {
                            "name": "iOS Distribution: Test Team",
                            "certificateType": "IOS_DISTRIBUTION",
                            "displayName": "Test Team",
                            "serialNumber": "12345678",
                            "platform": "IOS",
                            "expirationDate": "2027-11-07T00:00:00.000+0000",
                            "certificateContent": STANDARD.encode(cert),
                        }
                    },
                    "links": {"self": "https://api.appstoreconnect.apple.com/v1/certificates"}
                }))
            })
            .expect(1)
            .mount(server)
            .await;
    }

    /// В ответе нет ни ключа, ни пароля, ни CSR, ни содержимого сертификата.
    fn assert_no_secrets(out: &Value, vault: &MemoryVault, csr: &str) {
        let text = out.to_string();
        assert!(!text.contains("-----BEGIN"), "{text}");
        assert!(!text.contains("certificateContent"), "{text}");
        assert!(!text.contains("MII"), "{text}");
        for account in [
            private_key_account("CERT123ABC"),
            p12_password_account("CERT123ABC"),
        ] {
            if let Some(secret) = vault.value(&account) {
                assert!(!text.contains(secret.trim()), "{account} leaked");
            }
        }
        let csr_body: String = csr.lines().filter(|l| !l.starts_with("-----")).collect();
        assert!(!csr_body.is_empty() && !text.contains(&csr_body[..40]));
    }

    #[tokio::test]
    async fn creates_certificate_stores_key_and_writes_p12_with_generated_password() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(MemoryVault::default());
        let csr_seen = Arc::new(std::sync::Mutex::new(String::new()));
        mock_apple(&server, csr_seen.clone()).await;
        let ctx = context(&server, dir.path(), vault.clone(), AccessLevel::Write);

        let out = create(
            &ctx,
            json!({"certificate_type": "IOS_DISTRIBUTION", "save_to": "dist.p12",
                   "common_name": "CI signing"}),
        )
        .await
        .unwrap();

        assert_eq!(out["id"], "CERT123ABC");
        assert_eq!(out["certificate_type"], "IOS_DISTRIBUTION");
        assert_eq!(out["serial_number"], "12345678");
        assert_eq!(out["expiration_date"], "2027-11-07T00:00:00.000+0000");
        assert_eq!(
            out["private_key_vault_entry"],
            "certificate/CERT123ABC/private-key"
        );
        assert_eq!(
            out["p12_password_vault_entry"],
            "certificate/CERT123ABC/p12-password"
        );

        // CSR: CN из аргументов, подпись — сгенерированным ключом (тем, что в хранилище).
        let csr_pem = csr_seen.lock().unwrap().clone();
        assert!(csr_pem.starts_with("-----BEGIN CERTIFICATE REQUEST-----"));
        let csr = CertReq::from_pem(&csr_pem).unwrap();
        assert_eq!(csr.info.subject.to_string(), "CN=CI signing");
        assert!(csr.info.attributes.is_empty());
        let key_pem = vault.value(&private_key_account("CERT123ABC")).unwrap();
        let key = <rsa::PrivateKey as pkcs8::DecodePrivateKey>::from_pkcs8_pem(&key_pem).unwrap();
        assert_eq!(
            SubjectPublicKeyInfoOwned::from_key(&key.to_public_key()).unwrap(),
            csr.info.public_key
        );
        let verifying = VerifyingKey::<Sha256>::new(key.to_public_key());
        let signature = Signature::try_from(csr.signature.raw_bytes()).unwrap();
        verifying
            .verify(&csr.info.to_der().unwrap(), &signature)
            .unwrap();

        // .p12 читается обратно с паролем из хранилища и содержит тот же ключ.
        let password = vault.value(&p12_password_account("CERT123ABC")).unwrap();
        assert_eq!(password.len(), PASSWORD_LEN);
        let p12 = std::fs::read(dir.path().join("dist.p12")).unwrap();
        assert_eq!(out["bytes"], p12.len());
        let store = KeyStore::from_pkcs12(&p12, &password, Pkcs12ImportPolicy::Strict).unwrap();
        let (_, chain) = store.private_key_chain().unwrap();
        assert_eq!(chain.key().as_der(), key.to_pkcs8_der().unwrap().as_bytes());
        assert_eq!(chain.certs().len(), 1);
        assert!(KeyStore::from_pkcs12(&p12, "wrong", Pkcs12ImportPolicy::Strict).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("dist.p12"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        assert_no_secrets(&out, &vault, &csr_pem);
    }

    #[tokio::test]
    async fn uses_password_file_and_does_not_store_it() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pass.txt"), "correct horse battery\n").unwrap();
        let vault = Arc::new(MemoryVault::default());
        let csr_seen = Arc::new(std::sync::Mutex::new(String::new()));
        mock_apple(&server, csr_seen.clone()).await;
        let ctx = context(&server, dir.path(), vault.clone(), AccessLevel::Write);

        let out = create(
            &ctx,
            json!({"certificate_type": "IOS_DISTRIBUTION", "save_to": "dist.p12",
                   "p12_password_file": "pass.txt"}),
        )
        .await
        .unwrap();

        assert_eq!(out["p12_password"], "from p12_password_file");
        assert!(out.get("p12_password_vault_entry").is_none());
        assert!(!vault.contains(&p12_password_account("CERT123ABC")).unwrap());
        assert!(vault.contains(&private_key_account("CERT123ABC")).unwrap());
        let csr = CertReq::from_pem(csr_seen.lock().unwrap().as_str()).unwrap();
        assert_eq!(csr.info.subject.to_string(), "CN=appstore-connect-mcp");
        let p12 = std::fs::read(dir.path().join("dist.p12")).unwrap();
        KeyStore::from_pkcs12(&p12, "correct horse battery", Pkcs12ImportPolicy::Strict).unwrap();
        assert!(!out.to_string().contains("correct horse"));
    }

    #[tokio::test]
    async fn apple_errors_store_nothing() {
        for status in [409u16, 400] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/certificates"))
                .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                    "errors": [{
                        "status": status.to_string(),
                        "code": "ENTITY_ERROR",
                        "title": "There is a problem with the request entity",
                        "detail": "You already have a current Distribution certificate."
                    }]
                })))
                .expect(1)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let vault = Arc::new(CountingVault::default());
            let ctx = context(&server, dir.path(), vault.clone(), AccessLevel::Write);

            let err = create(
                &ctx,
                json!({"certificate_type": "IOS_DISTRIBUTION", "save_to": "dist.p12"}),
            )
            .await
            .unwrap_err();

            assert!(err.message.contains(&status.to_string()), "{err}");
            assert!(!err.message.contains("-----BEGIN"), "{err}");
            assert_eq!(vault.stores.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn local_problems_are_reported_before_calling_apple() {
        let server = MockServer::start().await;
        // Ни один запрос не должен дойти до Apple.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exists.p12"), "x").unwrap();
        std::fs::write(dir.path().join("empty.txt"), "\n").unwrap();
        let vault = Arc::new(MemoryVault::default());
        let ctx = context(&server, dir.path(), vault, AccessLevel::Write);

        for (args, expected) in [
            (
                json!({"certificate_type": "IOS_DISTRIBUTION", "save_to": "exists.p12"}),
                "already exists",
            ),
            (
                json!({"certificate_type": "IOS_DISTRIBUTION", "p12_password_file": "x.txt"}),
                "requires `save_to`",
            ),
            (
                json!({"certificate_type": "IOS_DISTRIBUTION", "save_to": "a.p12",
                       "p12_password_file": "empty.txt"}),
                "is empty",
            ),
            (
                json!({"certificate_type": "IOS_DISTRIBUTION", "common_name": "a,O=evil"}),
                "invalid common_name",
            ),
            (
                json!({"certificate_type": "NOT_A_TYPE"}),
                "unknown certificate_type",
            ),
        ] {
            let err = create(&ctx, args).await.unwrap_err();
            assert!(err.message.contains(expected), "{err}");
        }
    }

    #[test]
    fn schema_takes_certificate_types_from_the_spec() {
        let types = certificate_types().unwrap();
        assert!(types.iter().any(|t| t == "IOS_DISTRIBUTION"));
        assert!(types.iter().any(|t| t == "DEVELOPER_ID_APPLICATION_G2"));
        let schema = input_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["certificate_type"]["enum"],
            json!(types)
        );
        let tool = &tools()[0];
        assert_eq!(tool.access, AccessLevel::Write);
        assert!(tool.uses_local_files);
    }
}
