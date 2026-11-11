//! `appstore-connect-mcp key exec` — ключ профиля для внешних инструментов (`xcrun altool`,
//! `notarytool`, fastlane) временным файлом.
//!
//! Ключ пишется в новый файл `AuthKey_<KEYID>.p8` с правами 0600 в новом каталоге с правами
//! 0700 (такое имя ищут `altool` и `notarytool`), команда запускается без оболочки с путём и
//! ID в переменных окружения, её stdout и stderr проходят через [`mask::Masker`]. После
//! завершения команды файл и каталог удаляются — и при ошибке, и при ненулевом коде, и при
//! Ctrl-C/SIGTERM: Ctrl-C команда получает от терминала сама (общая группа процессов), а мы
//! его только переживаем; SIGTERM и SIGHUP пересылаются команде. От SIGKILL защиты нет: файл
//! останется во временном каталоге пользователя, доступный только ему.
//!
//! Ключ в переменные окружения не кладётся — только путь к файлу. Аргументы команды мы не
//! раскрываем: `$APP_STORE_CONNECT_KEY_PATH` раскрывает оболочка дочерней команды
//! (`-- sh -c '…'`).

mod mask;

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use anyhow::{Context, bail};
use appstore_connect_api::auth::KeyringStore;
use appstore_connect_api::{Issuer, KeyId, PrivateKeyPem};
use clap::{Args, Subcommand};
use tokio::io::AsyncWrite;
use tokio::process::Command;

use self::mask::{Masker, needles_for_key, pump};
use super::ProfileArg;
use crate::profiles::{ProfileRepo, StoredKey, load_stored_key_with};

/// Путь к временному файлу ключа.
pub const KEY_PATH_ENV: &str = "APP_STORE_CONNECT_KEY_PATH";
/// Key ID.
pub const KEY_ID_ENV: &str = "APP_STORE_CONNECT_KEY_ID";
/// Issuer ID командного ключа; для индивидуального ключа не задаётся.
pub const ISSUER_ID_ENV: &str = "APP_STORE_CONNECT_ISSUER_ID";
/// Каталог с `AuthKey_<KEYID>.p8`, где ключ ищет `xcrun altool --apiKey <KEYID>`.
pub const KEYS_DIR_ENV: &str = "API_PRIVATE_KEYS_DIR";
/// Те же значения под именами действия fastlane `app_store_connect_api_key`.
pub const FASTLANE_KEY_PATH_ENV: &str = "APP_STORE_CONNECT_API_KEY_KEY_FILEPATH";
pub const FASTLANE_KEY_ID_ENV: &str = "APP_STORE_CONNECT_API_KEY_KEY_ID";
pub const FASTLANE_ISSUER_ID_ENV: &str = "APP_STORE_CONNECT_API_KEY_ISSUER_ID";
/// Переменные fastlane с содержимым ключа: унаследованные от родителя убираются, чтобы
/// fastlane не взял вместо файла чужой ключ (или не отказался из-за двух ключей сразу).
const FASTLANE_KEY_CONTENT_ENVS: [&str; 2] = [
    "APP_STORE_CONNECT_API_KEY_KEY",
    "APP_STORE_CONNECT_API_KEY_IS_KEY_CONTENT_BASE64",
];

/// Префикс имени временного каталога.
const TEMP_DIR_PREFIX: &str = "appstore-connect-mcp-key-";

const EXEC_HELP: &str = "\
The API key of the profile is written to a new temporary file AuthKey_<KEYID>.p8 (mode 0600, \
in a new directory with mode 0700) and the command runs with these environment variables:
  APP_STORE_CONNECT_KEY_PATH              path to the key file
  APP_STORE_CONNECT_KEY_ID                Key ID
  APP_STORE_CONNECT_ISSUER_ID             Issuer ID (not set for an individual key)
  API_PRIVATE_KEYS_DIR                    directory of the key file (xcrun altool --apiKey)
  APP_STORE_CONNECT_API_KEY_KEY_FILEPATH  the same path, KEY_ID and ISSUER_ID under the names
  APP_STORE_CONNECT_API_KEY_KEY_ID        of the fastlane action app_store_connect_api_key
  APP_STORE_CONNECT_API_KEY_ISSUER_ID
The key itself is never put into the environment; inherited APP_STORE_CONNECT_API_KEY_KEY and \
APP_STORE_CONNECT_API_KEY_IS_KEY_CONTENT_BASE64 are removed.

The file and its directory are deleted when the command exits, also when it fails or is \
interrupted: Ctrl-C reaches the command through the terminal, SIGTERM and SIGHUP are forwarded \
to it. The exit code of the command is returned.

The command runs directly, without a shell; its stdin is inherited. The key (as a whole, line \
by line and in base64) is replaced with *** in its stdout and stderr. To use the variables in \
arguments, run a shell yourself, in single quotes, so that the child shell expands them:
  appstore-connect-mcp key exec --profile work -- sh -c 'xcrun notarytool submit App.zip \\
    --key \"$APP_STORE_CONNECT_KEY_PATH\" --key-id \"$APP_STORE_CONNECT_KEY_ID\" \\
    --issuer \"$APP_STORE_CONNECT_ISSUER_ID\" --wait'
  appstore-connect-mcp key exec --profile work -- fastlane ios beta

On Windows there are no file modes: the file and its directory inherit the permissions of the \
temporary folder (%TEMP%, under your user profile and closed to other users by default). \
Signals are not forwarded there. Without sh, run PowerShell the same way and use \
$env:APP_STORE_CONNECT_KEY_PATH inside the quotes.

Never print the key: masking is a safety net against accidental output, not a security \
boundary.";

#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Run a command with the API key of the profile in a temporary file (mode 0600) that is
    /// deleted when the command exits.
    #[command(after_help = EXEC_HELP)]
    Exec(ExecArgs),
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<OsString>,
}

pub async fn run(command: KeyCommand) -> anyhow::Result<ExitCode> {
    match command {
        KeyCommand::Exec(args) => {
            let repo = ProfileRepo::open_default()?;
            let stored = load_stored_key_with(&repo, &KeyringStore, &args.profile.profile)?;
            // Обработчики ставятся до создания файла: сигнал между записью ключа и запуском
            // команды не должен убить процесс, оставив файл на диске.
            let mut signals = ProcessSignals::new()?;
            let code = exec_with_key(
                &std::env::temp_dir(),
                &stored,
                &args.command,
                tokio::io::stdout(),
                tokio::io::stderr(),
                &mut signals,
            )
            .await?;
            Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
        }
    }
}

/// Сигнал, который пересылается дочерней команде.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum Forward {
    Terminate,
    Hangup,
}

/// Источник сигналов для пересылки; в тестах подменяется каналом, чтобы не трогать
/// обработчики сигналов процесса тестов.
pub trait SignalSource {
    /// Следующий сигнал для пересылки; `None` — сигналов больше не будет.
    async fn recv(&mut self) -> Option<Forward>;
}

/// Сигналы процесса.
#[cfg(unix)]
pub struct ProcessSignals {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ProcessSignals {
    pub fn new() -> anyhow::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        let context = "cannot install signal handlers";
        Ok(Self {
            term: signal(SignalKind::terminate()).context(context)?,
            hup: signal(SignalKind::hangup()).context(context)?,
            int: signal(SignalKind::interrupt()).context(context)?,
        })
    }
}

#[cfg(unix)]
impl SignalSource for ProcessSignals {
    async fn recv(&mut self) -> Option<Forward> {
        loop {
            tokio::select! {
                got = self.term.recv() => return got.map(|()| Forward::Terminate),
                got = self.hup.recv() => return got.map(|()| Forward::Hangup),
                // Ctrl-C команда получает от терминала сама; повторная пересылка дала бы ей
                // два сигнала. Сами переживаем его, чтобы дождаться команды и удалить файл.
                got = self.int.recv() => got?,
            }
        }
    }
}

/// Сигналы процесса: вне Unix пересылать нечего, Ctrl-C только переживаем.
#[cfg(not(unix))]
pub struct ProcessSignals;

#[cfg(not(unix))]
impl ProcessSignals {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self)
    }
}

#[cfg(not(unix))]
impl SignalSource for ProcessSignals {
    async fn recv(&mut self) -> Option<Forward> {
        while tokio::signal::ctrl_c().await.is_ok() {}
        None
    }
}

/// Пересылает сигнал дочернему процессу. Процесс ещё не «пожат» (`wait` не завершён),
/// поэтому его PID не может достаться другому процессу.
#[cfg(unix)]
fn forward(pid: Option<u32>, signal: Forward) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) else {
        return;
    };
    let signal = match signal {
        Forward::Terminate => Signal::SIGTERM,
        Forward::Hangup => Signal::SIGHUP,
    };
    // Процесс мог уже завершиться: ошибка `ESRCH` не важна.
    let _ = kill(Pid::from_raw(pid), signal);
}

#[cfg(not(unix))]
fn forward(_pid: Option<u32>, _signal: Forward) {}

/// Временный файл ключа в собственном каталоге. Каталог удаляется и при `drop` (паника,
/// ранний выход), но обычный путь — [`KeyFile::remove`] с проверкой результата.
pub struct KeyFile {
    dir: tempfile::TempDir,
    path: PathBuf,
}

impl KeyFile {
    /// Создаёт в `root` новый каталог (0700) и в нём новый файл `AuthKey_<KEYID>.p8` (0600)
    /// с ключом.
    pub fn create(root: &Path, pem: &PrivateKeyPem, key_id: &KeyId) -> anyhow::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix(TEMP_DIR_PREFIX)
            .tempdir_in(root)
            .context("cannot create a temporary directory for the key file")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
                .context("cannot restrict the temporary directory to the current user")?;
        }
        // Key ID — только буквы и цифры (`KeyId::new`), имя файла безопасно.
        let path = dir.path().join(format!("AuthKey_{}.p8", key_id.as_str()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .with_context(|| format!("cannot create `{}`", path.display()))?;
        // Права задаются явно ещё раз: `mode` при создании урезается umask (только сужает,
        // но так права не зависят от umask вовсе).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .with_context(|| format!("cannot set the mode of `{}`", path.display()))?;
        }
        file.write_all(pem.expose().as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        Ok(Self { dir, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Удаляет файл и каталог.
    pub fn remove(self) -> anyhow::Result<()> {
        let dir = self.dir.path().to_path_buf();
        self.dir.close().with_context(|| {
            format!(
                "could not delete the temporary key directory {}; delete it yourself",
                dir.display()
            )
        })
    }
}

/// Переменные окружения команды: что задать и что убрать из унаследованных.
pub struct KeyEnv {
    pub set: Vec<(&'static str, OsString)>,
    pub remove: Vec<&'static str>,
}

pub fn key_env(file: &KeyFile, key_id: &KeyId, issuer: &Issuer) -> KeyEnv {
    let path = file.path().as_os_str().to_os_string();
    let mut set = vec![
        (KEY_PATH_ENV, path.clone()),
        (FASTLANE_KEY_PATH_ENV, path),
        (KEYS_DIR_ENV, file.dir().as_os_str().to_os_string()),
        (KEY_ID_ENV, key_id.as_str().into()),
        (FASTLANE_KEY_ID_ENV, key_id.as_str().into()),
    ];
    let mut remove = FASTLANE_KEY_CONTENT_ENVS.to_vec();
    match issuer {
        Issuer::Team(id) => {
            set.push((ISSUER_ID_ENV, id.as_str().into()));
            set.push((FASTLANE_ISSUER_ID_ENV, id.as_str().into()));
        }
        // Унаследованный Issuer ID относился бы к другому ключу.
        Issuer::Individual => remove.extend([ISSUER_ID_ENV, FASTLANE_ISSUER_ID_ENV]),
    }
    KeyEnv { set, remove }
}

/// Пишет ключ во временный файл в `temp_root`, запускает команду и удаляет файл. Возвращает
/// код завершения команды (для завершения сигналом на Unix — `128 + номер сигнала`).
pub async fn exec_with_key<O, E, S>(
    temp_root: &Path,
    key: &StoredKey,
    command: &[OsString],
    stdout: O,
    stderr: E,
    signals: &mut S,
) -> anyhow::Result<i32>
where
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
    S: SignalSource,
{
    let Some((program, args)) = command.split_first() else {
        bail!("no command given: pass it after `--`");
    };
    let file = KeyFile::create(temp_root, &key.pem, &key.key_id)?;
    let env = key_env(&file, &key.key_id, &key.issuer);
    let needles = needles_for_key(key.pem.expose());
    let result = run_masked(program, args, &env, &needles, stdout, stderr, signals).await;
    let removed = file.remove();
    match (result, removed) {
        (Ok(code), Ok(())) => Ok(code),
        (Ok(code), Err(err)) => Err(err.context(format!("the command exited with code {code}"))),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(removal)) => Err(err.context(format!("{removal:#}"))),
    }
}

/// Запускает команду, копирует её вывод с маскированием и пересылает ей сигналы.
async fn run_masked<O, E, S>(
    program: &OsString,
    args: &[OsString],
    env: &KeyEnv,
    needles: &[Vec<u8>],
    stdout: O,
    stderr: E,
    signals: &mut S,
) -> anyhow::Result<i32>
where
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
    S: SignalSource,
{
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.set.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in &env.remove {
        command.env_remove(name);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot run `{}`", program.to_string_lossy()))?;
    let pid = child.id();
    let child_out = child.stdout.take().context("child stdout is not piped")?;
    let child_err = child.stderr.take().context("child stderr is not piped")?;

    let wait = async {
        let mut signals_open = true;
        loop {
            tokio::select! {
                status = child.wait() => return status,
                signal = signals.recv(), if signals_open => match signal {
                    Some(signal) => forward(pid, signal),
                    None => signals_open = false,
                },
            }
        }
    };
    let (status, out_result, err_result) = tokio::join!(
        wait,
        pump(child_out, stdout, Masker::new(needles.iter().cloned())),
        pump(child_err, stderr, Masker::new(needles.iter().cloned())),
    );
    for (stream, result) in [("stdout", out_result), ("stderr", err_result)] {
        if let Err(err) = result {
            // Закрытый канал (`| head`) — обычная ситуация конвейера.
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                eprintln!(
                    "appstore-connect-mcp: warning: copying the command {stream} failed: {err}"
                );
            }
        }
    }
    let status = status.context("waiting for the command failed")?;
    Ok(exit_code(&status))
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

#[cfg(all(test, unix))]
mod tests {
    use appstore_connect_api::IssuerId;
    use tokio::sync::mpsc;

    use super::*;
    use crate::testing::generated_key_pem;

    const KEY_ID: &str = "TESTKEY123";
    const ISSUER: &str = "01234567-89ab-cdef-0123-456789abcdef";

    /// Сигналы из канала: обработчики сигналов процесса тестов не трогаются.
    struct ChannelSignals(mpsc::UnboundedReceiver<Forward>);

    impl SignalSource for ChannelSignals {
        async fn recv(&mut self) -> Option<Forward> {
            self.0.recv().await
        }
    }

    fn channel() -> (mpsc::UnboundedSender<Forward>, ChannelSignals) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, ChannelSignals(rx))
    }

    fn team() -> Issuer {
        Issuer::Team(IssuerId::new(ISSUER).unwrap())
    }

    fn key_id() -> KeyId {
        KeyId::new(KEY_ID).unwrap()
    }

    fn strings(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    struct Run {
        code: i32,
        out: String,
        err: String,
    }

    /// Запуск во временном корне; после запуска в корне ничего не должно остаться.
    struct Exec {
        root: tempfile::TempDir,
        pem: String,
        issuer: Issuer,
    }

    impl Exec {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
                pem: generated_key_pem(),
                issuer: team(),
            }
        }

        async fn command(
            &self,
            command: &[OsString],
            signals: &mut ChannelSignals,
        ) -> anyhow::Result<Run> {
            let key = StoredKey {
                settings: Default::default(),
                key_id: key_id(),
                issuer: self.issuer.clone(),
                pem: PrivateKeyPem::parse(&self.pem).unwrap(),
            };
            let mut out = Vec::new();
            let mut err = Vec::new();
            let result =
                exec_with_key(self.root.path(), &key, command, &mut out, &mut err, signals).await;
            assert!(
                fs::read_dir(self.root.path()).unwrap().next().is_none(),
                "the temporary key directory is left behind"
            );
            Ok(Run {
                code: result?,
                out: String::from_utf8(out).unwrap(),
                err: String::from_utf8(err).unwrap(),
            })
        }

        async fn sh(&self, script: &str) -> Run {
            let (_tx, mut signals) = channel();
            self.command(&strings(&["sh", "-c", script]), &mut signals)
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn child_gets_env_and_private_key_file() {
        let exec = Exec::new();
        let run = exec
            .sh(r#"echo "$APP_STORE_CONNECT_KEY_PATH"
echo "id=$APP_STORE_CONNECT_KEY_ID issuer=$APP_STORE_CONNECT_ISSUER_ID"
echo "fastlane=$APP_STORE_CONNECT_API_KEY_KEY_ID $APP_STORE_CONNECT_API_KEY_ISSUER_ID"
test "$APP_STORE_CONNECT_API_KEY_KEY_FILEPATH" = "$APP_STORE_CONNECT_KEY_PATH" && echo same-path
test "$API_PRIVATE_KEYS_DIR/AuthKey_TESTKEY123.p8" = "$APP_STORE_CONNECT_KEY_PATH" && echo keys-dir
ls -ld "$APP_STORE_CONNECT_KEY_PATH" "$API_PRIVATE_KEYS_DIR" | cut -c1-10"#)
            .await;
        assert_eq!(run.code, 0, "{}", run.err);
        let path = PathBuf::from(run.out.lines().next().unwrap());
        assert_eq!(path.file_name().unwrap(), "AuthKey_TESTKEY123.p8");
        let text = run.out;
        assert!(
            text.contains(&format!("id={KEY_ID} issuer={ISSUER}")),
            "{text}"
        );
        assert!(
            text.contains(&format!("fastlane={KEY_ID} {ISSUER}")),
            "{text}"
        );
        assert!(text.contains("same-path"), "{text}");
        assert!(text.contains("keys-dir"), "{text}");
        assert!(text.contains("-rw-------"), "{text}");
        assert!(text.contains("drwx------"), "{text}");
    }

    #[tokio::test]
    async fn child_reads_the_key_and_output_is_masked() {
        let exec = Exec::new();
        let body_line = exec.pem.lines().nth(1).unwrap().to_owned();
        let run = exec
            .sh(r#"cat "$APP_STORE_CONNECT_KEY_PATH"; echo; sed -n 2p "$APP_STORE_CONNECT_KEY_PATH" >&2
grep -v -e ----- "$APP_STORE_CONNECT_KEY_PATH" | tr -d '\n'; echo
head -n 1 "$APP_STORE_CONNECT_KEY_PATH"; wc -c < "$APP_STORE_CONNECT_KEY_PATH"
env | grep -c 'PRIVATE KEY'"#)
            .await;
        assert_eq!(run.code, 1, "grep -c finds nothing: {}", run.err);
        let lines: Vec<&str> = run.out.lines().collect();
        // Весь файл и тело одной строкой скрыты.
        assert_eq!(lines[0], "***", "{}", run.out);
        assert_eq!(lines[1], "***", "{}", run.out);
        // Первая строка PEM не секрет; размер файла — как у ключа; в окружении ключа нет.
        assert_eq!(lines[2], "-----BEGIN PRIVATE KEY-----");
        assert_eq!(lines[3].trim(), exec.pem.len().to_string());
        assert_eq!(lines[4].trim(), "0");
        assert_eq!(run.err, "***\n");
        assert!(!run.out.contains(&body_line), "{}", run.out);
    }

    #[tokio::test]
    async fn individual_key_has_no_issuer() {
        let mut exec = Exec::new();
        exec.issuer = Issuer::Individual;
        let run = exec
            .sh(r#"echo "[${APP_STORE_CONNECT_ISSUER_ID-unset}] [${APP_STORE_CONNECT_API_KEY_ISSUER_ID-unset}] [$APP_STORE_CONNECT_KEY_ID]""#)
            .await;
        assert_eq!(run.out.trim(), format!("[unset] [unset] [{KEY_ID}]"));
    }

    #[test]
    fn inherited_key_variables_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let pem = PrivateKeyPem::parse(&generated_key_pem()).unwrap();
        let file = KeyFile::create(root.path(), &pem, &key_id()).unwrap();
        let team_env = key_env(&file, &key_id(), &team());
        assert_eq!(team_env.remove, FASTLANE_KEY_CONTENT_ENVS);
        let names: Vec<&str> = team_env.set.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            [
                KEY_PATH_ENV,
                FASTLANE_KEY_PATH_ENV,
                KEYS_DIR_ENV,
                KEY_ID_ENV,
                FASTLANE_KEY_ID_ENV,
                ISSUER_ID_ENV,
                FASTLANE_ISSUER_ID_ENV
            ]
        );
        let individual = key_env(&file, &key_id(), &Issuer::Individual);
        assert!(individual.remove.contains(&ISSUER_ID_ENV));
        assert!(individual.remove.contains(&FASTLANE_ISSUER_ID_ENV));
        assert!(
            individual
                .set
                .iter()
                .all(|(name, _)| !name.contains("ISSUER"))
        );
        file.remove().unwrap();
    }

    #[tokio::test]
    async fn file_is_removed_after_failure() {
        let exec = Exec::new();
        let run = exec
            .sh("cat \"$APP_STORE_CONNECT_KEY_PATH\" >&2; exit 7")
            .await;
        assert_eq!(run.code, 7);
        assert_eq!(run.err, "***");

        // Команда не запустилась: ошибка, файла тоже нет (это проверяет `Exec::command`).
        let (_tx, mut signals) = channel();
        let err = exec
            .command(&strings(&["/nonexistent/program"]), &mut signals)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("cannot run `/nonexistent/program`"), "{err}");

        let err = exec
            .command(&[], &mut signals)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("no command"), "{err}");
    }

    #[tokio::test]
    async fn signals_are_forwarded_and_file_is_removed() {
        let exec = Exec::new();
        let (tx, mut signals) = channel();
        let sender = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tx.send(Forward::Terminate).unwrap();
            // Канал остаётся открытым, пока команда не завершится.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let started = std::time::Instant::now();
        let run = exec
            .command(&strings(&["sh", "-c", "exec sleep 20"]), &mut signals)
            .await
            .unwrap();
        sender.abort();
        assert_eq!(run.code, 128 + 15);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));

        // Закрытый источник сигналов не мешает дождаться команды.
        let (tx, mut signals) = channel();
        drop(tx);
        let run = exec
            .command(&strings(&["sh", "-c", "exit 3"]), &mut signals)
            .await
            .unwrap();
        assert_eq!(run.code, 3);
    }

    #[test]
    fn key_file_is_created_new_with_private_modes() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let pem = PrivateKeyPem::parse(&generated_key_pem()).unwrap();
        let file = KeyFile::create(root.path(), &pem, &key_id()).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(file.path()), 0o600);
        assert_eq!(mode(file.dir()), 0o700);
        assert_eq!(fs::read_to_string(file.path()).unwrap(), pem.expose());
        // Второй файл с тем же Key ID — в своём каталоге, первый не перезаписывается.
        let second = KeyFile::create(root.path(), &pem, &key_id()).unwrap();
        assert_ne!(second.path(), file.path());
        file.remove().unwrap();
        second.remove().unwrap();
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }
}
