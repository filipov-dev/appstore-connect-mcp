//! Claude Code: регистрация через `claude mcp add`.
//!
//! Конфигурацию Claude Code (`~/.claude.json`, `.mcp.json`) напрямую не правим — только через
//! его CLI, чтобы не зависеть от внутреннего формата файлов и не задеть чужие записи.

use std::io;
use std::process::Command;

use anyhow::{Context, bail};

use super::{Existing, Scope, ServerSpec, shell_join};
use crate::ui::Ui;

/// Имя исполняемого файла Claude Code CLI.
pub const CLAUDE_PROGRAM: &str = "claude";

/// Результат внешней команды.
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Запуск внешних команд; в тестах подменяется записью вызовов.
pub trait CommandRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput>;
}

/// Запуск через `std::process::Command`.
#[derive(Debug, Default)]
pub struct SystemRunner;

/// Запасное имя программы, если `program` не нашлась.
///
/// На Windows `Command` ищет в PATH только `<имя>.exe`, а установка Claude Code через npm
/// кладёт обёртку `claude.cmd`. Rust экранирует аргументы для `.cmd` сам и отказывается
/// запускать, если безопасно экранировать нельзя. В других ОС и при других ошибках запасного
/// имени нет.
fn windows_cmd_fallback(program: &str, windows: bool, err: &io::Error) -> Option<String> {
    (windows && err.kind() == io::ErrorKind::NotFound && !program.contains('.'))
        .then(|| format!("{program}.cmd"))
}

impl CommandRunner for SystemRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
        let output = match Command::new(program).args(args).output() {
            Err(err) => match windows_cmd_fallback(program, cfg!(windows), &err) {
                Some(fallback) => Command::new(fallback).args(args).output()?,
                None => return Err(err),
            },
            Ok(output) => output,
        };
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Аргументы `claude mcp add --scope <scope> <name> -- <exe> serve --profile <p> ...`.
pub fn add_args(scope: Scope, spec: &ServerSpec) -> Vec<String> {
    let mut args = strings(&[
        "mcp",
        "add",
        "--scope",
        scope.as_str(),
        spec.name.as_str(),
        "--",
    ]);
    args.push(spec.command.clone());
    args.extend(spec.args.iter().cloned());
    args
}

/// Аргументы `claude mcp remove --scope <scope> <name>`.
pub fn remove_args(scope: Scope, name: &str) -> Vec<String> {
    strings(&["mcp", "remove", "--scope", scope.as_str(), name])
}

/// Аргументы `claude mcp get <name>`.
pub fn get_args(name: &str) -> Vec<String> {
    strings(&["mcp", "get", name])
}

fn display(args: &[String]) -> String {
    shell_join(std::iter::once(CLAUDE_PROGRAM).chain(args.iter().map(String::as_str)))
}

/// Проверяет, что Claude Code CLI доступен, до первого вопроса пользователю.
pub fn ensure_available(runner: &mut dyn CommandRunner) -> anyhow::Result<()> {
    match runner.run(CLAUDE_PROGRAM, &["--version".to_owned()]) {
        Ok(output) if output.success => Ok(()),
        Ok(output) => bail!(
            "`{CLAUDE_PROGRAM} --version` failed: {}",
            output.stderr.trim()
        ),
        Err(err) if err.kind() == io::ErrorKind::NotFound => bail!(
            "Claude Code CLI `{CLAUDE_PROGRAM}` was not found in PATH; install Claude Code first"
        ),
        Err(err) => Err(err).context("failed to run Claude Code CLI"),
    }
}

/// Сообщение Claude Code о том, что сервер с таким именем уже есть.
fn already_exists(output: &CommandOutput) -> bool {
    let text = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    text.contains("already exists")
}

/// Вид существующей записи по строкам `Command:` и `Args:` в выводе `claude mcp get`.
/// Если вывод не разобрать, запись считается чужой.
fn existing_entry(output: &CommandOutput, profile: &str) -> Existing {
    if !output.success {
        return Existing::Foreign;
    }
    let field = |name: &str| {
        output
            .stdout
            .lines()
            .find_map(|line| line.trim().strip_prefix(name).map(str::trim))
    };
    let args = field("Args:").unwrap_or_default();
    Existing::classify(field("Command:"), args.split_whitespace(), profile)
}

fn run_checked(runner: &mut dyn CommandRunner, args: &[String]) -> anyhow::Result<CommandOutput> {
    runner
        .run(CLAUDE_PROGRAM, args)
        .with_context(|| format!("failed to run `{}`", display(args)))
}

/// Регистрирует сервер в Claude Code. Возвращает `false`, если пользователь отказался.
///
/// Своя запись с тем же именем и профилем заменяется после подтверждения (или сразу с `yes`).
/// Чужая запись или запись другого профиля (другого кабинета) заменяется только после явного
/// ответа в терминале: `yes` её не перезаписывает.
pub fn register(
    runner: &mut dyn CommandRunner,
    ui: &mut dyn Ui,
    scope: Scope,
    spec: &ServerSpec,
    yes: bool,
) -> anyhow::Result<bool> {
    let name = spec.name.as_str();
    let add = add_args(scope, spec);
    ui.say(&format!(
        "Claude Code ({} scope) will get MCP server `{name}`:\n  {}",
        scope.as_str(),
        display(&add)
    ));
    if !yes && !ui.confirm("Register the server in Claude Code?", false)? {
        return Ok(false);
    }

    let output = run_checked(runner, &add)?;
    if output.success {
        return Ok(true);
    }
    if !already_exists(&output) {
        bail!("`{}` failed: {}", display(&add), output.stderr.trim());
    }

    let remove = remove_args(scope, name);
    let existing = existing_entry(&run_checked(runner, &get_args(name))?, &spec.profile);
    ui.say(&format!(
        "Claude Code already has MCP server `{name}` ({}). It will be replaced:\n  {}\n  {}",
        existing.describe(),
        display(&remove),
        display(&add)
    ));
    if existing == Existing::SameProfile {
        if !yes && !ui.confirm("Replace the existing entry?", false)? {
            return Ok(false);
        }
    } else {
        ui.say("To keep that entry, re-run connect with another --name.");
        if yes {
            bail!(
                "Claude Code already has MCP server `{name}` ({}); it was not changed. Choose \
                 another name with --name, or re-run without --yes to replace it",
                existing.describe()
            );
        }
        if !ui.confirm(&format!("Replace the other server `{name}`?"), false)? {
            return Ok(false);
        }
    }
    let output = run_checked(runner, &remove)?;
    if !output.success {
        bail!("`{}` failed: {}", display(&remove), output.stderr.trim());
    }
    let output = run_checked(runner, &add)?;
    if !output.success {
        bail!("`{}` failed: {}", display(&add), output.stderr.trim());
    }
    Ok(true)
}

#[cfg(test)]
pub mod testing {
    use std::collections::VecDeque;

    use super::*;

    /// Записывает вызовы и возвращает заранее заданные результаты; `claude` не запускается.
    #[derive(Debug, Default)]
    pub struct FakeRunner {
        pub calls: Vec<Vec<String>>,
        pub results: VecDeque<CommandOutput>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
            assert_eq!(program, CLAUDE_PROGRAM);
            self.calls.push(args.to_vec());
            Ok(self.results.pop_front().unwrap_or(CommandOutput {
                success: true,
                ..Default::default()
            }))
        }
    }

    /// Ответ `claude mcp add`, когда сервер с таким именем уже есть.
    pub fn exists() -> CommandOutput {
        CommandOutput {
            success: false,
            stderr: "MCP server appstore-connect already exists in user config".into(),
            ..Default::default()
        }
    }

    /// Ответ `claude mcp get` для записи с командой `command` и профилем `profile`.
    pub fn get_output(command: &str, profile: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            stdout: format!(
                "appstore-connect:\n  Scope: User config (available in all your projects)\n  \
                 Status: ✓ Connected\n  Type: stdio\n  Command: {command}\n  \
                 Args: serve --profile {profile} --dynamic-toolsets\n  Environment:\n"
            ),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeRunner, exists, get_output};
    use super::*;
    use crate::clients::TEST_EXE;
    use crate::ui::testing::ScriptedUi;

    fn spec() -> ServerSpec {
        ServerSpec {
            name: "appstore-connect".into(),
            profile: "default".into(),
            command: TEST_EXE.into(),
            args: vec!["serve".into(), "--profile".into(), "default".into()],
        }
    }

    fn answers(values: &[&str]) -> ScriptedUi {
        ScriptedUi {
            answers: values.iter().map(|value| (*value).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn windows_falls_back_to_cmd_wrapper() {
        let not_found = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(
            windows_cmd_fallback(CLAUDE_PROGRAM, true, &not_found).as_deref(),
            Some("claude.cmd")
        );
        assert_eq!(
            windows_cmd_fallback(CLAUDE_PROGRAM, false, &not_found),
            None
        );
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(windows_cmd_fallback(CLAUDE_PROGRAM, true, &denied), None);
        assert_eq!(windows_cmd_fallback("claude.cmd", true, &not_found), None);
    }

    #[test]
    fn builds_argv() {
        assert_eq!(
            add_args(Scope::User, &spec()),
            [
                "mcp",
                "add",
                "--scope",
                "user",
                "appstore-connect",
                "--",
                TEST_EXE,
                "serve",
                "--profile",
                "default"
            ]
        );
        assert_eq!(
            remove_args(Scope::Project, "asc-x"),
            ["mcp", "remove", "--scope", "project", "asc-x"]
        );
        assert_eq!(get_args("asc-x"), ["mcp", "get", "asc-x"]);
    }

    #[test]
    fn parses_existing_entries() {
        let own = "/opt/homebrew/bin/appstore-connect-mcp";
        assert_eq!(
            existing_entry(&get_output(own, "default"), "default"),
            Existing::SameProfile
        );
        assert_eq!(
            existing_entry(&get_output(own, "work"), "default"),
            Existing::OtherProfile("work".into())
        );
        assert_eq!(
            existing_entry(&get_output("npx", "default"), "default"),
            Existing::Foreign
        );
        assert_eq!(
            existing_entry(&CommandOutput::default(), "default"),
            Existing::Foreign
        );
    }

    #[test]
    fn register_with_confirmation() {
        let mut runner = FakeRunner::default();
        let mut ui = answers(&["y"]);
        assert!(register(&mut runner, &mut ui, Scope::User, &spec(), false).unwrap());
        assert_eq!(runner.calls, [add_args(Scope::User, &spec())]);
        assert!(
            ui.text()
                .contains("claude mcp add --scope user appstore-connect --")
        );
    }

    #[test]
    fn declined_or_unanswered_registration_runs_nothing() {
        for ui in [answers(&["n"]), answers(&[""])] {
            let mut ui = ui;
            let mut runner = FakeRunner::default();
            assert!(!register(&mut runner, &mut ui, Scope::User, &spec(), false).unwrap());
            assert!(runner.calls.is_empty());
        }
        // Нет ответа (EOF) — ошибка ввода, конфигурация не меняется.
        let mut runner = FakeRunner::default();
        let mut ui = ScriptedUi::default();
        assert!(register(&mut runner, &mut ui, Scope::User, &spec(), false).is_err());
        assert!(runner.calls.is_empty());
    }

    #[test]
    fn replaces_own_entry_of_the_same_profile() {
        let mut runner = FakeRunner {
            results: [
                exists(),
                get_output("/opt/homebrew/bin/appstore-connect-mcp", "default"),
            ]
            .into(),
            ..Default::default()
        };
        let mut ui = ScriptedUi::default();
        assert!(register(&mut runner, &mut ui, Scope::User, &spec(), true).unwrap());
        assert_eq!(
            runner.calls,
            [
                add_args(Scope::User, &spec()),
                get_args("appstore-connect"),
                remove_args(Scope::User, "appstore-connect"),
                add_args(Scope::User, &spec())
            ]
        );
        assert!(ui.questions.is_empty());
    }

    #[test]
    fn foreign_or_other_profile_entry_is_not_replaced_with_yes() {
        for existing in [
            get_output("node", "default"),
            get_output("/opt/homebrew/bin/appstore-connect-mcp", "work"),
        ] {
            let mut runner = FakeRunner {
                results: [exists(), existing.clone()].into(),
                ..Default::default()
            };
            let mut ui = ScriptedUi::default();
            let err = register(&mut runner, &mut ui, Scope::User, &spec(), true).unwrap_err();
            assert!(err.to_string().contains("--name"), "{err}");
            assert_eq!(
                runner.calls,
                [add_args(Scope::User, &spec()), get_args("appstore-connect")]
            );

            // Интерактивно — только после явного «да»; отказ ничего не удаляет.
            let mut runner = FakeRunner {
                results: [exists(), existing.clone()].into(),
                ..Default::default()
            };
            let mut ui = answers(&["y", "n"]);
            assert!(!register(&mut runner, &mut ui, Scope::User, &spec(), false).unwrap());
            assert_eq!(runner.calls.len(), 2);

            let mut runner = FakeRunner {
                results: [exists(), existing].into(),
                ..Default::default()
            };
            let mut ui = answers(&["y", "y"]);
            assert!(register(&mut runner, &mut ui, Scope::User, &spec(), false).unwrap());
            assert_eq!(runner.calls.len(), 4);
        }
    }

    #[test]
    fn other_profile_is_named() {
        let mut runner = FakeRunner {
            results: [
                exists(),
                get_output("/opt/homebrew/bin/appstore-connect-mcp", "work"),
            ]
            .into(),
            ..Default::default()
        };
        let mut ui = answers(&["y", "n"]);
        register(&mut runner, &mut ui, Scope::User, &spec(), false).unwrap();
        assert!(
            ui.text().contains("another profile, `work`"),
            "{}",
            ui.text()
        );
    }

    #[test]
    fn other_failures_are_errors() {
        let mut runner = FakeRunner {
            results: [CommandOutput {
                success: false,
                stderr: "boom".into(),
                ..Default::default()
            }]
            .into(),
            ..Default::default()
        };
        let mut ui = ScriptedUi::default();
        let err = register(&mut runner, &mut ui, Scope::User, &spec(), true).unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert_eq!(runner.calls.len(), 1);
    }
}
