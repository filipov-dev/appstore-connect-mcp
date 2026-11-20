//! Регистрация MCP-сервера в клиентах (Claude Code, Codex).

pub mod claude;
pub mod codex;

use std::path::Path;

use anyhow::{Context, bail};
use clap::ValueEnum;

use crate::profiles::{DEFAULT_PROFILE, ProfileSettings};

/// Абсолютный путь к бинарнику для тестов. В Windows путь без буквы диска не абсолютный,
/// поэтому `/usr/local/bin/...` там не годится.
#[cfg(test)]
pub(crate) const TEST_EXE: &str = if cfg!(windows) {
    r"C:\tools\appstore-connect-mcp.exe"
} else {
    "/usr/local/bin/appstore-connect-mcp"
};

/// Имя сервера в конфигурации MCP-клиентов для профиля `default`.
pub const DEFAULT_SERVER_NAME: &str = "appstore-connect";

/// Префикс имени сервера для остальных профилей: `asc-<профиль>`.
///
/// Короткий, потому что клиенты называют инструменты `mcp__<сервер>__<инструмент>` и
/// ограничивают это имя 64 символами (см. [`MAX_SAFE_SERVER_NAME_LEN`]).
pub const PROFILE_SERVER_PREFIX: &str = "asc-";

/// Имя исполняемого файла `appstore-connect-mcp` без расширения: по нему узнаём свои записи
/// в конфигурации клиентов.
pub const EXE_STEM: &str = "appstore-connect-mcp";

/// Максимальная длина имени сервера.
const MAX_SERVER_NAME_LEN: usize = 64;

/// Предел длины полного имени инструмента у клиентов (`mcp__<сервер>__<инструмент>`):
/// имя инструмента в API модели — не длиннее 64 символов.
pub const CLIENT_TOOL_NAME_LIMIT: usize = 64;

/// Самое длинное имя инструмента: генератор операций отклоняет имена длиннее 40 символов.
pub const MAX_TOOL_NAME_LEN: usize = 40;

/// Самое длинное имя сервера, при котором любой инструмент укладывается в
/// [`CLIENT_TOOL_NAME_LIMIT`]: 64 − 40 − `mcp__` − `__` = 17. `appstore-connect` (16) влезает.
pub const MAX_SAFE_SERVER_NAME_LEN: usize =
    CLIENT_TOOL_NAME_LIMIT - MAX_TOOL_NAME_LEN - "mcp__".len() - "__".len();

/// Поддерживаемые MCP-клиенты.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ClientKind {
    /// Claude Code (`claude mcp add`).
    ClaudeCode,
    /// OpenAI Codex (`$CODEX_HOME/config.toml`).
    Codex,
}

impl ClientKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

/// Область конфигурации клиента.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Scope {
    /// Available in all projects of the current user.
    #[default]
    User,
    /// Shared with the project (Claude Code: `.mcp.json` in the current directory).
    Project,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// Имя сервера по умолчанию для профиля: `appstore-connect` для `default`, иначе
/// `asc-<профиль>` (`.` в имени профиля заменяется на `-`: в имени сервера точки нельзя).
pub fn default_server_name(profile: &str) -> String {
    if profile == DEFAULT_PROFILE {
        DEFAULT_SERVER_NAME.to_owned()
    } else {
        format!("{PROFILE_SERVER_PREFIX}{}", profile.replace('.', "-"))
    }
}

/// Предупреждение о длинном имени сервера: полные имена самых длинных инструментов
/// (`mcp__<сервер>__<инструмент>`) не уложатся в предел клиентов.
pub fn long_name_warning(name: &str) -> Option<String> {
    (name.len() > MAX_SAFE_SERVER_NAME_LEN).then(|| {
        format!(
            "Warning: server name `{name}` is longer than {MAX_SAFE_SERVER_NAME_LEN} characters. \
             Clients name tools mcp__<server>__<tool> and limit that to \
             {CLIENT_TOOL_NAME_LIMIT} characters, so tools with long names (up to \
             {MAX_TOOL_NAME_LEN} characters) may be shortened or rejected. Choose a shorter \
             name with --name."
        )
    })
}

/// Проверяет имя сервера в конфигурации клиента: оно становится ключом TOML Codex и
/// аргументом `claude mcp add`, поэтому допускаются только `[A-Za-z0-9_-]`, без `-` в начале.
pub fn validate_server_name(name: &str) -> anyhow::Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    if name.is_empty() || name.len() > MAX_SERVER_NAME_LEN || !valid_chars || name.starts_with('-')
    {
        bail!(
            "invalid server name `{name}`: use 1-{MAX_SERVER_NAME_LEN} characters from A-Z, a-z, 0-9, `_`, `-`, not starting with `-`; choose one with --name"
        );
    }
    Ok(())
}

/// Команда запуска сервера, которую получает MCP-клиент. Ключа в ней нет: `serve` берёт его
/// из системного хранилища по профилю.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// Имя сервера в конфигурации клиента.
    pub name: String,
    /// Профиль, который запускает сервер (`serve --profile`).
    pub profile: String,
    pub command: String,
    pub args: Vec<String>,
}

impl ServerSpec {
    /// `exe` — абсолютный путь к бинарнику. Путь не канонизируется: при установке через
    /// Homebrew symlink в `bin/` переживает обновление версии, а путь в `Cellar` — нет.
    pub fn new(
        name: &str,
        exe: &Path,
        profile: &str,
        settings: &ProfileSettings,
    ) -> anyhow::Result<Self> {
        validate_server_name(name)?;
        if !exe.is_absolute() {
            bail!("executable path `{}` is not absolute", exe.display());
        }
        let command = exe
            .to_str()
            .with_context(|| format!("executable path `{}` is not valid UTF-8", exe.display()))?
            .to_owned();

        let mut args = vec![
            "serve".to_owned(),
            "--profile".to_owned(),
            profile.to_owned(),
        ];
        // Флагов доступа нет: права — настройка профиля, флаги `serve` их только сужают.
        if !settings.toolsets.is_empty() {
            args.push("--toolsets".to_owned());
            args.push(settings.toolsets.join(","));
        }
        if settings.dynamic_toolsets {
            args.push("--dynamic-toolsets".to_owned());
        }
        Ok(Self {
            name: name.to_owned(),
            profile: profile.to_owned(),
            command,
            args,
        })
    }

    /// Команда целиком для показа пользователю.
    pub fn display(&self) -> String {
        shell_join(
            std::iter::once(self.command.as_str()).chain(self.args.iter().map(String::as_str)),
        )
    }
}

/// Запускает ли команда `command` бинарник `appstore-connect-mcp` (по имени файла, с `.exe`
/// или без).
pub fn is_own_command(command: &str) -> bool {
    // Разделители обеих систем: конфиг мог быть записан на другой ОС.
    let file = command.rsplit(['/', '\\']).next().unwrap_or(command);
    // Имя с `.exe` — путь Windows, где регистр в именах файлов не различается. Без `.exe`
    // имя сравнивается точно.
    match file.len().checked_sub(".exe".len()) {
        Some(split)
            if file.is_char_boundary(split) && file[split..].eq_ignore_ascii_case(".exe") =>
        {
            file[..split].eq_ignore_ascii_case(EXE_STEM)
        }
        _ => file == EXE_STEM,
    }
}

/// Профиль из аргументов своей записи (`serve --profile <имя>`); без `--profile` — `default`,
/// как у `serve`.
pub fn profile_in_args<'a>(args: impl IntoIterator<Item = &'a str>) -> String {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--profile" {
            if let Some(value) = args.next() {
                return value.to_owned();
            }
        } else if let Some(value) = arg.strip_prefix("--profile=") {
            return value.to_owned();
        }
    }
    DEFAULT_PROFILE.to_owned()
}

/// Прежняя запись с тем же именем.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Existing {
    /// Запись запускает `appstore-connect-mcp` с тем же профилем: её можно заменить.
    SameProfile,
    /// Запись запускает `appstore-connect-mcp` с другим профилем (другим кабинетом).
    OtherProfile(String),
    /// Другой сервер.
    Foreign,
}

impl Existing {
    /// Определяет вид записи по её команде и аргументам.
    pub fn classify<'a>(
        command: Option<&str>,
        args: impl IntoIterator<Item = &'a str>,
        profile: &str,
    ) -> Self {
        if !command.is_some_and(is_own_command) {
            return Self::Foreign;
        }
        let existing = profile_in_args(args);
        if existing == profile {
            Self::SameProfile
        } else {
            Self::OtherProfile(existing)
        }
    }

    /// Описание для пользователя.
    pub fn describe(&self) -> String {
        match self {
            Self::SameProfile => "appstore-connect-mcp, the same profile".to_owned(),
            Self::OtherProfile(profile) => {
                format!("appstore-connect-mcp with another profile, `{profile}`")
            }
            Self::Foreign => "another server, not appstore-connect-mcp".to_owned(),
        }
    }
}

/// Склеивает аргументы в строку для показа, экранируя их как в POSIX shell.
pub fn shell_join<'a>(args: impl IntoIterator<Item = &'a str>) -> String {
    args.into_iter()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '/' | ',' | ':' | '=' | '@' | '+')
        });
    if safe {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn server_spec_args() {
        let exe = PathBuf::from(TEST_EXE);
        let spec = ServerSpec::new("asc-x", &exe, "x", &ProfileSettings::default()).unwrap();
        assert_eq!(spec.name, "asc-x");
        assert_eq!(spec.profile, "x");
        assert_eq!(spec.command, TEST_EXE);
        assert_eq!(spec.args, ["serve", "--profile", "x"]);

        let settings = ProfileSettings {
            allow_write: true,
            allow_destructive: true,
            toolsets: vec!["apps".into(), "testflight".into()],
            dynamic_toolsets: true,
            ..ProfileSettings::default()
        };
        let spec = ServerSpec::new("asc-ci", &exe, "ci", &settings).unwrap();
        // Права профиля в регистрацию не попадают.
        assert_eq!(
            spec.args,
            [
                "serve",
                "--profile",
                "ci",
                "--toolsets",
                "apps,testflight",
                "--dynamic-toolsets"
            ]
        );
    }

    #[test]
    fn server_spec_requires_absolute_path_and_valid_name() {
        let settings = ProfileSettings::default();
        assert!(ServerSpec::new("asc", Path::new("appstore-connect-mcp"), "x", &settings).is_err());
        let exe = PathBuf::from(TEST_EXE);
        for bad in ["", "-x", "a b", "a.b", "a/b", "магазин", &"x".repeat(65)] {
            assert!(ServerSpec::new(bad, &exe, "x", &settings).is_err(), "{bad}");
        }
        for good in ["appstore-connect", "asc_2", "asc-work"] {
            validate_server_name(good).unwrap();
        }
    }

    #[test]
    fn default_names_fit_the_tool_name_limit() {
        assert_eq!(MAX_SAFE_SERVER_NAME_LEN, 17);
        assert_eq!(default_server_name(DEFAULT_PROFILE), "appstore-connect");
        assert_eq!(default_server_name("work"), "asc-work");
        assert_eq!(default_server_name("a.b"), "asc-a-b");
        validate_server_name(&default_server_name("a.b")).unwrap();
        // `mcp__appstore-connect__` + 40 символов = 63.
        assert_eq!(
            "mcp__".len() + DEFAULT_SERVER_NAME.len() + "__".len() + MAX_TOOL_NAME_LEN,
            63
        );
        assert_eq!(long_name_warning(DEFAULT_SERVER_NAME), None);
        // `asc-` + 13 символов профиля — ещё в пределе, 14 — уже нет.
        assert_eq!(
            long_name_warning(&default_server_name("abcdefghijklm")),
            None
        );
        let warning = long_name_warning(&default_server_name("abcdefghijklmn")).unwrap();
        assert!(warning.contains("--name"), "{warning}");
        assert!(long_name_warning("appstore-connect-work").is_some());
    }

    #[test]
    fn recognizes_own_command() {
        assert!(is_own_command("/opt/homebrew/bin/appstore-connect-mcp"));
        assert!(is_own_command(r"C:\tools\appstore-connect-mcp.exe"));
        assert!(is_own_command("appstore-connect-mcp"));
        assert!(is_own_command(r"C:\Tools\APPSTORE-CONNECT-MCP.EXE"));
        assert!(is_own_command("C:/tools/AppStore-Connect-Mcp.Exe"));
        assert!(!is_own_command(r"C:\tools\appstore-connect-mcp-old.exe"));
        assert!(!is_own_command(r"C:\tools\appstore-connect-mcp.exe.bak"));
        assert!(!is_own_command(r"C:\appstore-connect-mcp\node.exe"));
        // Без `.exe` имя сравнивается точно: в Unix это другой файл.
        assert!(!is_own_command("/usr/local/bin/AppStore-Connect-MCP"));
        assert!(!is_own_command(".exe"));
        assert!(!is_own_command("/usr/bin/node"));
        assert!(!is_own_command("npx"));
    }

    #[test]
    fn classifies_existing_entries() {
        let own = Some("/opt/homebrew/bin/appstore-connect-mcp");
        assert_eq!(
            Existing::classify(own, ["serve", "--profile", "work"], "work"),
            Existing::SameProfile
        );
        assert_eq!(
            Existing::classify(own, ["serve", "--profile=work"], "work"),
            Existing::SameProfile
        );
        assert_eq!(
            Existing::classify(own, ["serve"], DEFAULT_PROFILE),
            Existing::SameProfile
        );
        assert_eq!(
            Existing::classify(own, ["serve", "--profile", "other"], "work"),
            Existing::OtherProfile("other".into())
        );
        assert_eq!(
            Existing::classify(Some("node"), ["serve", "--profile", "work"], "work"),
            Existing::Foreign
        );
        assert_eq!(Existing::classify(None, [], "work"), Existing::Foreign);
    }

    #[test]
    fn shell_join_quotes_unsafe_args() {
        assert_eq!(
            shell_join(["/My Apps/bin", "it's", "plain-arg"]),
            r"'/My Apps/bin' 'it'\''s' plain-arg"
        );
    }
}
