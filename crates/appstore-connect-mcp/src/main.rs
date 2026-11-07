//! Бинарник `appstore-connect-mcp`: CLI и stdio-транспорт MCP-сервера App Store Connect.
//!
//! `main` только разбирает аргументы и вызывает модуль подкоманды из [`cli`].

mod browser;
mod cli;
mod clients;
mod login;
mod profiles;
mod ui;
mod verify;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// MCP server and CLI for the App Store Connect API.
#[derive(Debug, Parser)]
#[command(name = "appstore-connect-mcp", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the App Store Connect API keys of profiles (stored in the system keychain).
    #[command(subcommand)]
    Auth(cli::auth::AuthCommand),
    /// Register the server in an MCP client (Claude Code, Codex), storing the API key of the
    /// profile first if needed.
    Connect(cli::connect::ConnectArgs),
    /// Use the stored API key with other tools without revealing it.
    #[command(subcommand)]
    Key(cli::key::KeyCommand),
    /// Run the MCP server over stdio (read-only unless --allow-write is given).
    Serve(cli::serve::ServeArgs),
    /// List the toolsets with the number of tools per access level.
    Toolsets(cli::toolsets::ToolsetsArgs),
    /// List and validate custom tools (tools.toml).
    #[command(subcommand)]
    Tools(cli::tools::ToolsCommand),
    /// Internal: evaluate one `jq` filter for `serve` (JSON on stdin and stdout).
    #[command(name = "jq-worker", hide = true)]
    JqWorker,
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    // Таблица операций встроена в бинарник; ссылка на неё не даёт линковщику её выбросить.
    let _ = appstore_connect_mcp_core::appstore_connect_api::spec::operations().len();
    match Cli::parse().command {
        Command::Auth(command) => cli::auth::run(command).await.map(|()| ExitCode::SUCCESS),
        Command::Connect(args) => cli::connect::run(args).await.map(|()| ExitCode::SUCCESS),
        // Код завершения `key exec` — код дочерней команды.
        Command::Key(command) => cli::key::run(command).await,
        Command::Serve(args) => cli::serve::run(args).await.map(|()| ExitCode::SUCCESS),
        Command::Toolsets(args) => cli::toolsets::run(args).map(|()| ExitCode::SUCCESS),
        Command::Tools(command) => cli::tools::run(command).map(|()| ExitCode::SUCCESS),
        Command::JqWorker => cli::jq_worker::run().map(|()| ExitCode::SUCCESS),
    }
}

#[cfg(test)]
mod testing {
    use p256::ecdsa::SigningKey;
    use p256::elliptic_curve::Generate;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};

    /// PEM нового ключа P-256, сгенерированного в тесте (настоящие ключи в тестах не нужны).
    pub fn generated_key_pem() -> String {
        SigningKey::generate()
            .to_pkcs8_pem(LineEnding::LF)
            .unwrap()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;
    use crate::cli::auth::AuthCommand;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    /// Флаги, на которые опираются CI-скрипты.
    #[test]
    fn parses_auth_flags() {
        let cli = Cli::try_parse_from([
            "appstore-connect-mcp",
            "auth",
            "login",
            "--profile",
            "ci",
            "--key-file",
            "test.p8",
            "--key-id",
            "TESTKEY123",
            "--issuer-id",
            "01234567-89ab-cdef-0123-456789abcdef",
            "--no-verify",
        ])
        .unwrap();
        let Command::Auth(AuthCommand::Login(args)) = cli.command else {
            panic!("expected auth login");
        };
        assert_eq!(args.profile.profile, "ci");
        assert!(args.no_verify);
        assert_eq!(args.key_id.as_deref(), Some("TESTKEY123"));

        for status in [
            [
                "appstore-connect-mcp",
                "auth",
                "status",
                "--profile",
                "ci",
                "--no-verify",
            ],
            [
                "appstore-connect-mcp",
                "auth",
                "status",
                "--profile",
                "ci",
                "--offline",
            ],
        ] {
            let cli = Cli::try_parse_from(status).unwrap();
            let Command::Auth(AuthCommand::Status(args)) = cli.command else {
                panic!("expected auth status");
            };
            assert!(args.no_verify);
        }
        assert!(
            Cli::try_parse_from(["appstore-connect-mcp", "auth", "logout", "--profile", "ci"])
                .is_ok()
        );
        // Командный и индивидуальный ключ одновременно — ошибка разбора.
        assert!(
            Cli::try_parse_from([
                "appstore-connect-mcp",
                "auth",
                "login",
                "--issuer-id",
                "01234567-89ab-cdef-0123-456789abcdef",
                "--individual",
            ])
            .is_err()
        );
        // Содержимое ключа аргументом не принимается.
        assert!(
            Cli::try_parse_from(["appstore-connect-mcp", "auth", "login", "--key", "x"]).is_err()
        );
    }

    #[test]
    fn parses_connect_flags() {
        let cli = Cli::try_parse_from([
            "appstore-connect-mcp",
            "connect",
            "codex",
            "--profile",
            "work",
            "--key-file",
            "AuthKey_TESTKEY123.p8",
            "--individual",
            "--allow-destructive",
            "--toolsets",
            "apps,testflight",
            "--no-dynamic-toolsets",
            "--yes",
            "--no-browser",
        ])
        .unwrap();
        let Command::Connect(args) = cli.command else {
            panic!("expected connect");
        };
        assert_eq!(args.client, clients::ClientKind::Codex);
        assert_eq!(args.profile.profile, "work");
        assert!(args.individual && args.allow_destructive && args.no_dynamic_toolsets);
        assert_eq!(args.toolsets, ["apps", "testflight"]);
        assert!(args.yes && args.no_browser);
        assert_eq!(args.scope, clients::Scope::User);

        let cli = Cli::try_parse_from([
            "appstore-connect-mcp",
            "connect",
            "claude-code",
            "--scope",
            "project",
            "--name",
            "asc-team",
        ])
        .unwrap();
        let Command::Connect(args) = cli.command else {
            panic!("expected connect");
        };
        assert_eq!(args.client, clients::ClientKind::ClaudeCode);
        assert_eq!(args.name.as_deref(), Some("asc-team"));
        assert!(
            Cli::try_parse_from([
                "appstore-connect-mcp",
                "connect",
                "codex",
                "--issuer-id",
                "01234567-89ab-cdef-0123-456789abcdef",
                "--individual",
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["appstore-connect-mcp", "connect", "cursor"]).is_err());
    }

    #[test]
    fn parses_key_exec() {
        let cli = Cli::try_parse_from([
            "appstore-connect-mcp",
            "key",
            "exec",
            "--profile",
            "work",
            "--",
            "sh",
            "-c",
            "echo \"$APP_STORE_CONNECT_KEY_PATH\"",
        ])
        .unwrap();
        let Command::Key(cli::key::KeyCommand::Exec(args)) = cli.command else {
            panic!("expected key exec");
        };
        assert_eq!(args.profile.profile, "work");
        assert_eq!(args.command.len(), 3);
        // Флаги команды после `--` не разбираются как наши.
        let cli = Cli::try_parse_from([
            "appstore-connect-mcp",
            "key",
            "exec",
            "--",
            "xcrun",
            "--profile",
        ])
        .unwrap();
        let Command::Key(cli::key::KeyCommand::Exec(args)) = cli.command else {
            panic!("expected key exec");
        };
        assert_eq!(args.command, ["xcrun", "--profile"]);
        // Без команды — ошибка разбора.
        assert!(Cli::try_parse_from(["appstore-connect-mcp", "key", "exec"]).is_err());
    }
}
