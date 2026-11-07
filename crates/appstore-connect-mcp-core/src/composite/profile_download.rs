//! Составной инструмент `profile_download`: профиль подготовки в файл и, на macOS, в каталог
//! профилей Xcode.
//!
//! `GET /v1/profiles/{id}` отдаёт профиль в `profileContent` (base64); содержимое
//! пишется в новый файл `save_to` и агенту не возвращается.
//!
//! Установка (`install`) — единственное место, где инструмент пишет вне правил локальных
//! файлов сервера: в фиксированный каталог профилей пользователя
//! `~/Library/Developer/Xcode/UserData/Provisioning Profiles/` (Xcode 16 и новее; старые
//! версии читали `~/Library/MobileDevice/Provisioning Profiles/`). Имя файла — UUID
//! профиля; существующий файл с другим содержимым не перезаписывается.

use std::path::{Path, PathBuf};

use appstore_connect_api::{
    AccessLevel, HttpMethod,
    executor::{ApiRequest, ResponseBody},
    path::render_path,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{
    CompositeTool, Context, ToolError, ToolFuture, ToolResult, certificate_create::write_new_file,
};

/// Каталог профилей относительно домашнего каталога (Xcode 16 и новее).
const PROFILES_DIR: &str = "Library/Developer/Xcode/UserData/Provisioning Profiles";
/// Поля профиля, которые нужны инструменту.
const PROFILE_FIELDS: &str =
    "name,platform,profileType,profileState,profileContent,uuid,expirationDate";

const DESCRIPTION: &str = "Download a provisioning profile by its id (`GET /v1/profiles/{id}`) \
and write its content to a new local file `save_to` (use the .mobileprovision extension, or \
.provisionprofile for macOS and Mac Catalyst profiles). With `install: true` (macOS only) the \
profile is also copied to the user's Xcode profiles directory \
`~/Library/Developer/Xcode/UserData/Provisioning Profiles/<UUID>.mobileprovision` (Xcode 16 and \
later; macOS profiles get .provisionprofile); an identical installed profile is left as is, a \
different file with the same name is not overwritten. Returns the profile id, name, type, \
state, UUID, expiration date and the written paths; the profile content itself is never \
returned. Find profile ids with `list_profiles`.";

pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: "profile_download",
        toolset: "provisioning",
        access: AccessLevel::Read,
        description: DESCRIPTION,
        input_schema,
        uses_local_files: true,
        run,
    }]
}

fn run<'a>(ctx: &'a Context, args: Value) -> ToolFuture<'a> {
    Box::pin(download(ctx, args))
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["profile_id", "save_to"],
        "properties": {
            "profile_id": {
                "type": "string",
                "description": "Profile resource id (the `id` from list_profiles), not the UUID."
            },
            "save_to": {
                "type": "string",
                "description": "Path of a new file for the profile, for example \
                    App_Store.mobileprovision. Existing files are not overwritten."
            },
            "install": {
                "type": "boolean",
                "default": false,
                "description": "macOS only: also copy the profile to \
                    ~/Library/Developer/Xcode/UserData/Provisioning Profiles/ so Xcode and \
                    xcodebuild find it."
            }
        }
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    profile_id: String,
    save_to: String,
    #[serde(default)]
    install: bool,
}

async fn download(ctx: &Context, args: Value) -> ToolResult {
    let args: Args = serde_json::from_value(args)
        .map_err(|e| ToolError::new(format!("invalid arguments: {e}")))?;
    if args.install && !cfg!(target_os = "macos") {
        return Err(ToolError::new(
            "`install` is supported only when the server runs on macOS; download the profile \
             with save_to and install it on the Mac",
        ));
    }
    let save_to = ctx.files.resolve_write(&args.save_to)?;

    let path = render_path("/v1/profiles/{id}", |name| {
        (name == "id").then(|| args.profile_id.clone())
    })?;
    let request =
        ApiRequest::new(HttpMethod::Get, path).with_query("fields[profiles]", PROFILE_FIELDS);
    let response = ctx.api.execute(request).await?;
    let ResponseBody::Json(body) = response.body else {
        return Err(ToolError::new(
            "App Store Connect returned an unexpected response for the profile",
        ));
    };
    let data = body.get("data").unwrap_or(&Value::Null);
    let attributes = data
        .get("attributes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let content = attributes
        .get("profileContent")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new("the profile response has no profileContent"))?;
    let compact: String = content.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = STANDARD
        .decode(compact)
        .map_err(|_| ToolError::new("the profile content is not valid base64"))?;

    write_new_file(&save_to, &bytes).await?;

    let mut out = summary(data, &attributes);
    out.insert("saved_to".into(), json!(save_to.display().to_string()));
    out.insert("bytes".into(), json!(bytes.len()));

    if args.install {
        let installed = install(&attributes, &bytes).await.map_err(|e| {
            ToolError::new(format!(
                "the profile was saved to {}, but it was not installed: {e}",
                save_to.display()
            ))
        })?;
        out.insert(
            "installed_to".into(),
            json!(installed.path.display().to_string()),
        );
        out.insert("already_installed".into(), json!(installed.already_present));
    }
    Ok(Value::Object(out))
}

/// Сводка профиля для ответа: без `profileContent`.
fn summary(data: &Value, attributes: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(id) = data.get("id") {
        out.insert("id".into(), id.clone());
    }
    for (api, ours) in [
        ("name", "name"),
        ("profileType", "profile_type"),
        ("platform", "platform"),
        ("profileState", "profile_state"),
        ("uuid", "uuid"),
        ("expirationDate", "expiration_date"),
    ] {
        if let Some(value) = attributes.get(api).filter(|v| !v.is_null()) {
            out.insert(ours.into(), value.clone());
        }
    }
    out
}

#[cfg(target_os = "macos")]
async fn install(attributes: &Map<String, Value>, bytes: &[u8]) -> Result<Installed, ToolError> {
    let home = dirs::home_dir()
        .ok_or_else(|| ToolError::new("the home directory of the user is unknown"))?;
    install_into(&home, attributes, bytes).await
}

#[cfg(not(target_os = "macos"))]
async fn install(_attributes: &Map<String, Value>, _bytes: &[u8]) -> Result<Installed, ToolError> {
    Err(ToolError::new("`install` is supported only on macOS"))
}

/// Установленный профиль.
#[derive(Debug)]
struct Installed {
    path: PathBuf,
    /// Такой же файл уже был: ничего не записано.
    already_present: bool,
}

/// Копия профиля в `<home>/Library/Developer/Xcode/UserData/Provisioning Profiles/`.
/// Домашний каталог передаётся явно, чтобы тесты не трогали настоящий `~/Library`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
async fn install_into(
    home: &Path,
    attributes: &Map<String, Value>,
    bytes: &[u8],
) -> Result<Installed, ToolError> {
    let uuid = attributes
        .get("uuid")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // UUID становится именем файла: только формат UUID, без разделителей пути.
    let valid_uuid = uuid.len() == 36
        && uuid.chars().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    if !valid_uuid {
        return Err(ToolError::new("the profile has no valid UUID"));
    }
    let profile_type = attributes
        .get("profileType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Xcode хранит профили macOS и Mac Catalyst как .provisionprofile.
    let extension = if profile_type.starts_with("MAC_") {
        "provisionprofile"
    } else {
        "mobileprovision"
    };
    let dir = home.join(PROFILES_DIR);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| ToolError::new(format!("could not create {}: {e}", dir.display())))?;
    let path = dir.join(format!("{uuid}.{extension}"));
    match tokio::fs::symlink_metadata(&path).await {
        Ok(meta) => {
            let same = meta.is_file()
                && tokio::fs::read(&path)
                    .await
                    .is_ok_and(|existing| existing == bytes);
            if same {
                return Ok(Installed {
                    path,
                    already_present: true,
                });
            }
            return Err(ToolError::new(format!(
                "{} already exists with different content; it was not overwritten. Remove it \
                 if it is stale and install again",
                path.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(ToolError::new(format!(
                "could not check {}: {e}",
                path.display()
            )));
        }
    }
    write_new_file(&path, bytes).await?;
    Ok(Installed {
        path,
        already_present: false,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    use super::*;
    use crate::composite::{certificate_create::tests::context, testing::MemoryVault};

    const UUID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

    /// Двоичное содержимое профиля (в настоящем — CMS-подпись Apple; здесь любые байты).
    fn profile_bytes() -> Vec<u8> {
        (0u8..=255).cycle().take(1000).collect()
    }

    async fn mock_profile(server: &MockServer, profile_type: &str) {
        Mock::given(method("GET"))
            .and(path("/v1/profiles/PROF123"))
            .and(query_param("fields[profiles]", PROFILE_FIELDS))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "type": "profiles",
                    "id": "PROF123",
                    "attributes": {
                        "name": "App Store com.example.app",
                        "platform": "IOS",
                        "profileType": profile_type,
                        "profileState": "ACTIVE",
                        "profileContent": STANDARD.encode(profile_bytes()),
                        "uuid": UUID,
                        "expirationDate": "2027-11-07T00:00:00.000+0000"
                    }
                },
                "links": {"self": "https://api.appstoreconnect.apple.com/v1/profiles/PROF123"}
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn writes_decoded_profile_and_returns_no_content() {
        let server = MockServer::start().await;
        mock_profile(&server, "IOS_APP_STORE").await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(
            &server,
            dir.path(),
            Arc::new(MemoryVault::default()),
            AccessLevel::Read,
        );

        let out = download(
            &ctx,
            json!({"profile_id": "PROF123", "save_to": "app.mobileprovision"}),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read(dir.path().join("app.mobileprovision")).unwrap(),
            profile_bytes()
        );
        assert_eq!(out["id"], "PROF123");
        assert_eq!(out["uuid"], UUID);
        assert_eq!(out["profile_type"], "IOS_APP_STORE");
        assert_eq!(out["expiration_date"], "2027-11-07T00:00:00.000+0000");
        assert_eq!(out["bytes"], 1000);
        assert!(out.get("installed_to").is_none());
        let encoded = STANDARD.encode(profile_bytes());
        assert!(!out.to_string().contains(&encoded[..32]));
    }

    #[tokio::test]
    async fn existing_file_is_refused_before_the_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.mobileprovision"), "old").unwrap();
        let ctx = context(
            &server,
            dir.path(),
            Arc::new(MemoryVault::default()),
            AccessLevel::Read,
        );

        let err = download(
            &ctx,
            json!({"profile_id": "PROF123", "save_to": "app.mobileprovision"}),
        )
        .await
        .unwrap_err();

        assert!(err.message.contains("already exists"), "{err}");
        assert_eq!(
            std::fs::read(dir.path().join("app.mobileprovision")).unwrap(),
            b"old"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn install_is_refused_outside_macos() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(
            &server,
            dir.path(),
            Arc::new(MemoryVault::default()),
            AccessLevel::Read,
        );
        let err = download(
            &ctx,
            json!({"profile_id": "PROF123", "save_to": "a.mobileprovision", "install": true}),
        )
        .await
        .unwrap_err();
        assert!(err.message.contains("only"), "{err}");
        assert!(!dir.path().join("a.mobileprovision").exists());
    }

    fn attributes(uuid: &str, profile_type: &str) -> Map<String, Value> {
        let value = json!({"uuid": uuid, "profileType": profile_type});
        value.as_object().cloned().unwrap()
    }

    #[tokio::test]
    async fn install_writes_into_the_xcode_profiles_directory_of_the_given_home() {
        let home = tempfile::tempdir().unwrap();
        let bytes = profile_bytes();
        let attrs = attributes(UUID, "IOS_APP_STORE");

        let first = install_into(home.path(), &attrs, &bytes).await.unwrap();
        let expected = home
            .path()
            .join("Library/Developer/Xcode/UserData/Provisioning Profiles")
            .join(format!("{UUID}.mobileprovision"));
        assert_eq!(first.path, expected);
        assert!(!first.already_present);
        assert_eq!(std::fs::read(&expected).unwrap(), bytes);

        // Тот же профиль ещё раз — ничего не меняется.
        let again = install_into(home.path(), &attrs, &bytes).await.unwrap();
        assert!(again.already_present);

        // Другое содержимое под тем же UUID не перезаписывается.
        let err = install_into(home.path(), &attrs, b"other")
            .await
            .unwrap_err();
        assert!(err.message.contains("not overwritten"), "{err}");
        assert_eq!(std::fs::read(&expected).unwrap(), bytes);

        // Профиль macOS — .provisionprofile.
        let mac = install_into(
            home.path(),
            &attributes("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE", "MAC_APP_STORE"),
            &bytes,
        )
        .await
        .unwrap();
        assert!(
            mac.path
                .ends_with("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE.provisionprofile")
        );
    }

    #[tokio::test]
    async fn install_rejects_uuids_that_are_not_file_names() {
        let home = tempfile::tempdir().unwrap();
        for uuid in [
            "",
            "../../../../etc/passwd",
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f/",
        ] {
            let err = install_into(home.path(), &attributes(uuid, "IOS_APP_STORE"), b"x")
                .await
                .unwrap_err();
            assert!(err.message.contains("UUID"), "{err}");
        }
        assert!(!home.path().join("Library").exists());
    }

    #[test]
    fn tool_is_read_only_and_uses_local_files() {
        let tool = &tools()[0];
        assert_eq!(tool.access, AccessLevel::Read);
        assert!(tool.uses_local_files);
        let schema = input_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["profile_id", "save_to"]));
    }
}
