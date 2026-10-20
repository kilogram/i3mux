//! Run commands and small file operations on a host: this machine, or a
//! remote machine over ssh.
//!
//! Guarantees:
//!
//! - Quoting happens only here (plan rule 9). `Host::Local` runs an argv with
//!   no shell. `Host::Ssh` joins the argv once with `shell_words` into a
//!   `bash -lc` script, and quotes that command once for the remote login
//!   shell that sshd starts. A remote login shell must parse POSIX single
//!   quotes (bash, zsh, dash, ksh).
//! - Remote commands run under `bash -l`, so `PATH` is the login `PATH`.
//! - An ssh transport failure (exit 255) is [`HostError::Unreachable`]; a
//!   command that ran and failed is [`HostError::Failed`].
//! - Every remote command first makes sure the host's ssh master runs
//!   ([`Masters::ensure_started`], plan-ssh-recovery rule 1). One function,
//!   [`HostRunner::ssh_argv`], builds every ssh client argv, for commands
//!   and windows alike: a client of the master that never starts a master
//!   itself, never forwards ports, and fails on a dead link instead of
//!   hanging (rule 4). A client that the master refuses connects on its own
//!   (rule 5).
//! - File paths are relative to the home directory of the host user. The
//!   file operations are POSIX `sh` scripts that run through [`HostExec::run`],
//!   so local and remote use the same code.
//! - Every write is atomic: it writes a temp file in the target directory
//!   and renames it, so readers see the old or the new file, never a
//!   partial one. Files are not fsynced (no portable `sh` way).
//! - The token of a session (plan-layout-backup rule 1a) is
//!   `$XDG_RUNTIME_DIR/i3mux/sessions/NAME.token` on the host, or
//!   `~/.local/state/i3mux/sessions/NAME.token` if that variable is unset or
//!   empty. The host resolves the directory: every token script starts with
//!   the one snippet `token_file_prelude!`. A host reboot empties the
//!   runtime directory, and so ends every session.
//! - Every token operation that writes runs as one script under an
//!   exclusive `flock` on `.lock` beside the token: take, checked write, and
//!   delete (rules 1, 1b and 4). So a take never lands between a checked
//!   write's compare and its write. No operation writes a file without
//!   checking a token.
//! - Output of the remote login profile never reaches the caller: a remote
//!   command prints [`START_MARKER`] after the profile has run, and
//!   [`HostExec::run`] drops everything up to and including it.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::master::{self, MasterError, Masters, LIVENESS_OPTIONS};
use crate::types::{Host, RemoteHost, SessionName, Token};

/// Exit status the file scripts use for "no such file" (the literal 44 in
/// `READ_SCRIPT`, `REMOVE_SCRIPT` and `READ_TOKEN_SCRIPT`).
const MISSING: i32 = 44;

/// Exit status of `CHECKED_WRITE_SCRIPT` when the token file holds another
/// token (the literal 45 in the script).
const OTHER_TOKEN: i32 = 45;

/// Exit status of `CHECKED_WRITE_SCRIPT` when a token is expected and there
/// is no token file (the literal 46 in the script).
const NO_TOKEN_FILE: i32 = 46;

/// ssh exits with 255 when the connection itself fails.
const SSH_TRANSPORT_FAILURE: i32 = 255;

/// Printed by every non-interactive remote command after the login profile
/// and before its own output. Record separators around a word that no
/// profile prints.
const START_MARKER: &str = "\u{1e}i3mux-start\u{1e}";

/// Shell code that prints [`START_MARKER`].
const PRINT_START_MARKER: &str = r"printf '\036i3mux-start\036'";

/// Run on remotes after the login profile, before the payload. shpool
/// installed with `cargo install` or from the release tarball lives in one
/// of these directories, and a `bash -l` profile does not always add them
/// (Arch with zsh as login shell: `~/.bash_profile` never reads `~/.profile`).
const REMOTE_PATH_PREFIX: &str = r#"PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"; export PATH;"#;

/// The terminal type remote shells get. Remote hosts often lack terminfo
/// for local terminals such as foot.
const REMOTE_TERM: &str = "TERM=xterm-256color";

const READ_SCRIPT: &str = r#"f="$HOME/$1"
[ -e "$f" ] || exit 44
exec cat -- "$f""#;

const LIST_SCRIPT: &str = r#"d="$HOME/$1"
[ -d "$d" ] || exit 0
cd "$d" || exit 1
for f in *; do
  if [ -f "$f" ]; then printf '%s\n' "$f"; fi
done"#;

const REMOVE_SCRIPT: &str = r#"f="$HOME/$1"
[ -e "$f" ] || exit 44
exec rm -f -- "$f""#;

/// Shell code that sets `td` to the token directory and `tok` to the token
/// file of session `$1` (plan-layout-backup rule 1a). Every token script
/// starts with it, so all of them agree on the location.
macro_rules! token_file_prelude {
    () => {
        r#"if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  td="$XDG_RUNTIME_DIR/i3mux/sessions"
else
  td="$HOME/.local/state/i3mux/sessions"
fi
tok="$td/$1.token"
"#
    };
}

/// `$1` is the session. Under the lock, replace its token with 16 bytes
/// from `/dev/urandom` as hex, then print them.
const TAKE_SCRIPT: &str = concat!(
    "set -e\n",
    token_file_prelude!(),
    r#"mkdir -p "$td"
exec 9>"$td/.lock"
flock 9
new=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
if [ "${#new}" -ne 32 ]; then
  echo "i3mux: cannot read 16 random bytes from /dev/urandom" >&2
  exit 1
fi
t=$(mktemp "$td/.${tok##*/}.XXXXXX")
trap 'rm -f "$t"' EXIT
printf '%s\n' "$new" > "$t"
mv -f "$t" "$tok"
printf '%s' "$new""#
);

/// `$1` is the session. Print its token, or exit 44 if it has none. No
/// lock: a take replaces the file by rename.
const READ_TOKEN_SCRIPT: &str = concat!(
    token_file_prelude!(),
    r#"[ -e "$tok" ] || exit 44
exec cat -- "$tok""#
);

/// `$1` is the session. Under the lock, delete its token file, if any.
const REMOVE_TOKEN_SCRIPT: &str = concat!(
    "set -e\n",
    token_file_prelude!(),
    r#"[ -d "$td" ] || exit 0
exec 9>"$td/.lock"
flock 9
rm -f -- "$tok""#
);

/// `$1` is the session, `$2` the expected token or empty for "no token
/// file", `$3` the file to write. Under the lock, compare, then write stdin
/// to `$3` atomically. Exit 45 for another token and 46 for no token file
/// when one is expected; both write nothing, but still read all of stdin,
/// so the writer never sees a broken pipe.
const CHECKED_WRITE_SCRIPT: &str = concat!(
    "set -e\n",
    token_file_prelude!(),
    r#"f="$HOME/$3"
d="${f%/*}"
mkdir -p "$d" "$td"
exec 9>"$td/.lock"
flock 9
if [ -e "$tok" ]; then
  found=$(cat -- "$tok")
  if [ "$found" != "$2" ]; then
    cat > /dev/null
    exit 45
  fi
elif [ -n "$2" ]; then
  cat > /dev/null
  exit 46
fi
t=$(mktemp "$d/.${f##*/}.XXXXXX")
trap 'rm -f "$t"' EXIT
cat > "$t"
mv -f "$t" "$f""#
);

/// What [`HostExec::write_file_if_token`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedWrite {
    /// The token file held the expected token, or there was none and none
    /// was expected: the file is written.
    Written,
    /// The token file holds another token. Nothing is written.
    OtherToken,
    /// A token was expected, but there is no token file. Nothing is written.
    NoTokenFile,
}

/// A command could not run on a host, or ran and failed.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("cannot reach {host} over ssh: {detail}")]
    Unreachable { host: RemoteHost, detail: String },
    #[error("command on {host} failed ({}): {}", exit_text(*.code), stderr.trim())]
    Failed {
        host: Host,
        /// Exit code; `None` if a signal ended the command.
        code: Option<i32>,
        stderr: String,
        /// The argv that ran, joined for display.
        command: String,
    },
    #[error("cannot start '{program}': {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the login shell on {host} did not run i3mux's command (no start marker in its output); \
         check that ~/.bash_profile does not exit or exec another program"
    )]
    NoStartMarker { host: RemoteHost },
    #[error("i3mux bug: tried to run an empty command")]
    EmptyCommand,
    #[error(transparent)]
    Master(#[from] MasterError),
}

fn exit_text(code: Option<i32>) -> String {
    code.map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}"))
}

/// The operations i3mux needs on a host. [`HostRunner`] is the real one;
/// tests use in-memory fakes.
pub trait HostExec {
    /// The host this runs on.
    fn host(&self) -> &Host;

    /// Run `argv` and return its stdout. `stdin`, if given, is written to
    /// the command's standard input; otherwise stdin is empty.
    ///
    /// # Errors
    /// [`HostError::Unreachable`] if ssh cannot connect, [`HostError::Failed`]
    /// if the command exits non-zero, [`HostError::Spawn`] if it cannot start,
    /// [`HostError::Master`] if the host's ssh master does not start.
    fn run(&self, argv: &[String], stdin: Option<&str>) -> Result<String, HostError>;

    /// The argv a local terminal runs to execute `argv` interactively on
    /// this host.
    fn attach_argv(&self, argv: &[String]) -> Vec<String>;

    /// The content of the file at `home_rel`, or `None` if it does not exist.
    ///
    /// # Errors
    /// Any [`HostError`] other than "missing".
    fn read_file(&self, home_rel: &str) -> Result<Option<String>, HostError>;

    /// Names of the regular, non-hidden files in directory `home_rel`.
    /// A missing directory has no files.
    ///
    /// # Errors
    /// Any [`HostError`].
    fn list_dir(&self, home_rel: &str) -> Result<Vec<String>, HostError>;

    /// Remove the file at `home_rel`. Returns whether it existed.
    ///
    /// # Errors
    /// Any [`HostError`].
    fn remove_file(&self, home_rel: &str) -> Result<bool, HostError>;

    /// Replace the token of `session` with a new one: 16 random bytes as 32
    /// lowercase hex digits. Returns what it wrote, unvalidated. Runs under
    /// the token lock.
    ///
    /// # Errors
    /// Any [`HostError`]; the token file is then unchanged.
    fn take_token(&self, session: &SessionName) -> Result<String, HostError>;

    /// The content of the token file of `session`, unvalidated, or `None` if
    /// it has none. Never writes.
    ///
    /// # Errors
    /// Any [`HostError`] other than "missing".
    fn read_token(&self, session: &SessionName) -> Result<Option<String>, HostError>;

    /// Delete the token file of `session`, if any, under the token lock.
    ///
    /// # Errors
    /// Any [`HostError`].
    fn remove_token(&self, session: &SessionName) -> Result<(), HostError>;

    /// Replace the file at `home_rel` with `content` atomically, creating
    /// parent directories, but only if the token file of `session` holds
    /// `token`, or does not exist when `token` is `None`. The compare and the
    /// write run under the token lock.
    ///
    /// # Errors
    /// Any [`HostError`]; the file is then unchanged.
    fn write_file_if_token(
        &self,
        home_rel: &str,
        content: &str,
        session: &SessionName,
        token: Option<&Token>,
    ) -> Result<CheckedWrite, HostError>;
}

/// Local settings a [`HostRunner`] needs.
#[derive(Clone)]
pub struct RunnerConfig {
    /// Home directory of the local user; exported as `HOME` to local commands.
    pub local_home: PathBuf,
    /// Runtime directory of the local user; exported as `XDG_RUNTIME_DIR` to
    /// local commands, or removed from their environment if `None`. Local
    /// token files live there (plan-layout-backup rule 1a).
    pub local_runtime_dir: Option<PathBuf>,
    /// Private (0700) directory for ssh control sockets.
    pub ssh_control_dir: PathBuf,
    /// Starts the ssh master of a remote host before each remote command.
    pub masters: Arc<dyn Masters + Send + Sync>,
}

/// Runs commands on a [`Host`] with real processes.
#[derive(Clone)]
pub struct HostRunner {
    host: Host,
    config: RunnerConfig,
}

impl HostRunner {
    #[must_use]
    pub fn new(host: Host, config: RunnerConfig) -> Self {
        Self { host, config }
    }

    /// `ssh ... HOST -- 'bash -lc ...'`, the argv of every ssh client
    /// (plan-ssh-recovery rule 4). With `tty`, a terminal is allocated and
    /// `TERM` is set for the remote side. Without it, the command prints
    /// [`START_MARKER`] before running `argv`.
    ///
    /// The client goes through the master's control socket and never starts
    /// a master (`ControlMaster=no`). Without a socket, or when the master
    /// refuses it (sshd `MaxSessions`), ssh connects on its own. Forwards
    /// belong to the master, so the client clears them: they would collide.
    fn ssh_argv(&self, remote: &RemoteHost, tty: bool, argv: &[String]) -> Vec<String> {
        let script = remote_script(argv, !tty);
        let control_socket = master::control_socket(&self.config.ssh_control_dir, remote);
        let mut ssh = Vec::new();
        if tty {
            ssh.extend(["env".to_owned(), REMOTE_TERM.to_owned()]);
        }
        ssh.extend(["ssh".to_owned(), "-S".to_owned()]);
        ssh.push(control_socket.display().to_string());
        ssh.extend(["-o", "ControlMaster=no", "-o", "ClearAllForwardings=yes"].map(str::to_owned));
        ssh.extend(LIVENESS_OPTIONS.map(str::to_owned));
        ssh.extend([
            if tty { "-t" } else { "-T" }.to_owned(),
            remote.as_str().to_owned(),
            "--".to_owned(),
            remote_command(&script),
        ]);
        ssh
    }

    /// The command that runs `argv` on the host. For a remote host, this
    /// first starts the host's ssh master unless it runs.
    fn command_for(&self, argv: &[String]) -> Result<Command, HostError> {
        let argv = match &self.host {
            Host::Local => argv.to_vec(),
            Host::Ssh(remote) => {
                self.config.masters.ensure_started(remote)?;
                self.ssh_argv(remote, false, argv)
            }
        };
        let (program, arguments) = argv.split_first().ok_or(HostError::EmptyCommand)?;
        let mut command = Command::new(program);
        command.args(arguments);
        if self.host == Host::Local {
            command.env("HOME", &self.config.local_home);
            match &self.config.local_runtime_dir {
                Some(dir) => command.env("XDG_RUNTIME_DIR", dir),
                None => command.env_remove("XDG_RUNTIME_DIR"),
            };
        }
        Ok(command)
    }

    fn run_script(
        &self,
        script: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<String, HostError> {
        self.run(&script_argv(script, args), stdin)
    }
}

/// `sh -c SCRIPT i3mux ARGS...`: the arguments reach the script as `$1`,
/// `$2`, ..., never parsed.
fn script_argv(script: &str, args: &[&str]) -> Vec<String> {
    ["sh", "-c", script, "i3mux"]
        .iter()
        .chain(args)
        .map(|a| (*a).to_owned())
        .collect()
}

/// The `bash -lc` script of a remote command: extend `PATH`, print
/// [`START_MARKER`] if `marked`, then run `argv` joined once.
fn remote_script(argv: &[String], marked: bool) -> String {
    let joined = shell_words::join(argv);
    if marked {
        format!("{REMOTE_PATH_PREFIX} {PRINT_START_MARKER}; {joined}")
    } else {
        format!("{REMOTE_PATH_PREFIX} {joined}")
    }
}

/// The remote command string ssh sends: `bash -lc '<script>'`.
///
/// sshd hands this string to the login shell, which parses it once; bash
/// then parses the script once. Each parse has its own quoting layer.
fn remote_command(script: &str) -> String {
    format!("bash -lc {}", shell_words::quote(script))
}

/// The output after the first [`START_MARKER`], or `None` if there is none.
fn after_start_marker(stdout: &str) -> Option<&str> {
    stdout.split_once(START_MARKER).map(|(_, rest)| rest)
}

/// Treat [`MISSING`] as `None`.
fn missing_as_none<T>(result: Result<T, HostError>) -> Result<Option<T>, HostError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(HostError::Failed {
            code: Some(MISSING),
            ..
        }) => Ok(None),
        Err(err) => Err(err),
    }
}

impl HostExec for HostRunner {
    fn host(&self) -> &Host {
        &self.host
    }

    fn run(&self, argv: &[String], stdin: Option<&str>) -> Result<String, HostError> {
        let command = self.command_for(argv)?;
        let output = run_captured(command, stdin)?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            return match &self.host {
                Host::Local => Ok(stdout.into_owned()),
                Host::Ssh(remote) => {
                    after_start_marker(&stdout)
                        .map(str::to_owned)
                        .ok_or_else(|| HostError::NoStartMarker {
                            host: remote.clone(),
                        })
                }
            };
        }
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        match (&self.host, output.status.code()) {
            (Host::Ssh(remote), Some(SSH_TRANSPORT_FAILURE)) => Err(HostError::Unreachable {
                host: remote.clone(),
                detail: stderr.trim().to_owned(),
            }),
            (_, code) => Err(HostError::Failed {
                host: self.host.clone(),
                code,
                stderr,
                command: shell_words::join(argv),
            }),
        }
    }

    fn attach_argv(&self, argv: &[String]) -> Vec<String> {
        match &self.host {
            Host::Local => argv.to_vec(),
            Host::Ssh(remote) => self.ssh_argv(remote, true, argv),
        }
    }

    fn read_file(&self, home_rel: &str) -> Result<Option<String>, HostError> {
        missing_as_none(self.run_script(READ_SCRIPT, &[home_rel], None))
    }

    fn list_dir(&self, home_rel: &str) -> Result<Vec<String>, HostError> {
        let out = self.run_script(LIST_SCRIPT, &[home_rel], None)?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    fn remove_file(&self, home_rel: &str) -> Result<bool, HostError> {
        missing_as_none(self.run_script(REMOVE_SCRIPT, &[home_rel], None)).map(|r| r.is_some())
    }

    fn take_token(&self, session: &SessionName) -> Result<String, HostError> {
        self.run_script(TAKE_SCRIPT, &[session.as_str()], None)
    }

    fn read_token(&self, session: &SessionName) -> Result<Option<String>, HostError> {
        missing_as_none(self.run_script(READ_TOKEN_SCRIPT, &[session.as_str()], None))
    }

    fn remove_token(&self, session: &SessionName) -> Result<(), HostError> {
        self.run_script(REMOVE_TOKEN_SCRIPT, &[session.as_str()], None)
            .map(drop)
    }

    fn write_file_if_token(
        &self,
        home_rel: &str,
        content: &str,
        session: &SessionName,
        token: Option<&Token>,
    ) -> Result<CheckedWrite, HostError> {
        // A token is never empty, so "" can stand for "no token file".
        let expected = token.map_or("", Token::as_str);
        let args = [session.as_str(), expected, home_rel];
        match self.run_script(CHECKED_WRITE_SCRIPT, &args, Some(content)) {
            Ok(_) => Ok(CheckedWrite::Written),
            Err(HostError::Failed {
                code: Some(OTHER_TOKEN),
                ..
            }) => Ok(CheckedWrite::OtherToken),
            Err(HostError::Failed {
                code: Some(NO_TOKEN_FILE),
                ..
            }) => Ok(CheckedWrite::NoTokenFile),
            Err(err) => Err(err),
        }
    }
}

/// Run `command` to completion with captured stdout and stderr.
///
/// The commands i3mux feeds on stdin print nothing until stdin closes, so
/// writing all input before reading output cannot deadlock.
fn run_captured(
    mut command: Command,
    stdin: Option<&str>,
) -> Result<std::process::Output, HostError> {
    let program = command.get_program().to_string_lossy().into_owned();
    let spawn_error = |source| HostError::Spawn {
        program: program.clone(),
        source,
    };
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(spawn_error)?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes()).map_err(spawn_error)?;
    }
    child.wait_with_output().map_err(spawn_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master::DirectSsh;
    use std::path::Path;

    /// This machine as a host, with `HOME` and `XDG_RUNTIME_DIR` (`run`)
    /// inside `home`.
    fn local(home: &Path) -> HostRunner {
        local_with_runtime_dir(home, Some(home.join("run")))
    }

    fn local_with_runtime_dir(home: &Path, runtime_dir: Option<PathBuf>) -> HostRunner {
        HostRunner::new(
            Host::Local,
            RunnerConfig {
                local_home: home.to_path_buf(),
                local_runtime_dir: runtime_dir,
                ssh_control_dir: home.join("ssh"),
                masters: Arc::new(DirectSsh),
            },
        )
    }

    fn remote(host: &str) -> HostRunner {
        HostRunner::new(
            Host::Ssh(RemoteHost::new(host).unwrap()),
            RunnerConfig {
                local_home: "/home/me".into(),
                local_runtime_dir: None,
                ssh_control_dir: "/run/user/1000/i3mux".into(),
                masters: Arc::new(DirectSsh),
            },
        )
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Run the command ssh would send through local `sh -c`, as sshd runs it
    /// through the login shell. `HOME` points at an empty directory so
    /// `bash -l` reads no user profile, and `XDG_RUNTIME_DIR` at `run` in it.
    fn run_like_sshd(remote_cmd: &str, home: &Path) -> String {
        let out = Command::new("sh")
            .arg("-c")
            .arg(remote_cmd)
            .env("HOME", home)
            .env("XDG_RUNTIME_DIR", home.join("run"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// The command an ssh `run` of `argv` sends, as a string.
    fn remote_run_command(argv: &[String]) -> String {
        remote("deepthought")
            .ssh_argv(&RemoteHost::new("deepthought").unwrap(), false, argv)
            .pop()
            .unwrap()
    }

    #[test]
    fn argv_with_quotes_and_dollar_survives_one_parse() {
        // Plan rule 9: a single argument with ' " $ and a space arrives intact.
        let tricky = "it's \"$HOME\" $(echo no) `echo no` \\ ; & | *";
        let argv = strings(&["printf", "%s|%s", tricky, "second arg"]);
        let home = tempfile::tempdir().unwrap();
        let printed = run_like_sshd(&remote_run_command(&argv), home.path());
        assert_eq!(
            after_start_marker(&printed),
            Some(format!("{tricky}|second arg").as_str())
        );
    }

    #[test]
    fn login_profile_output_is_discarded() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".bash_profile"),
            "echo 'Welcome to deepthought'\nprintf 'no newline {\"json\": 1}'\n",
        )
        .unwrap();
        let argv = strings(&["printf", "%s", "{\"sessions\": []}"]);
        let printed = run_like_sshd(&remote_run_command(&argv), home.path());
        assert!(printed.starts_with("Welcome to deepthought"), "{printed:?}");
        assert_eq!(after_start_marker(&printed), Some("{\"sessions\": []}"));
    }

    #[test]
    fn output_without_start_marker_is_rejected() {
        assert_eq!(after_start_marker("profile noise only"), None);
        assert_eq!(after_start_marker("a\u{1e}i3mux-start\u{1e}b"), Some("b"));
    }

    /// The command an interactive ssh attach of `argv` sends, as a string.
    fn remote_attach_command(argv: &[String]) -> String {
        remote("deepthought").attach_argv(argv).pop().unwrap()
    }

    #[test]
    fn remote_command_parses_back_to_argv() {
        let argv = strings(&["sh", "-c", "echo \"$1\"", "i3mux", "a'b\"c$ d"]);
        let words = shell_words::split(&remote_command(&shell_words::join(&argv))).unwrap();
        assert_eq!(words[..2], ["bash", "-lc"]);
        assert_eq!(shell_words::split(&words[2]).unwrap(), argv);
        for (command, prefix_words) in [
            (remote_run_command(&argv), 5),
            (remote_attach_command(&argv), 3),
        ] {
            let words = shell_words::split(&command).unwrap();
            let script = shell_words::split(&words[2]).unwrap();
            assert_eq!(script[prefix_words..], argv, "{command}");
        }
    }

    #[test]
    fn remote_path_export_precedes_the_payload() {
        let argv = strings(&["shpool", "attach", "ws4-001"]);
        for command in [remote_run_command(&argv), remote_attach_command(&argv)] {
            let script = &shell_words::split(&command).unwrap()[2];
            assert!(script.starts_with(REMOTE_PATH_PREFIX), "{script}");
            assert!(script.ends_with("shpool attach ws4-001"), "{script}");
        }
    }

    #[test]
    fn remote_commands_find_shpool_in_home_local_bin() {
        // A remote whose `bash -l` profile does not put ~/.local/bin on PATH
        // (the real case: Arch, zsh login shell, no ~/.bash_profile).
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        // Written by a child process: see mux::tests::run_prelude (ETXTBSY).
        let status = Command::new("sh")
            .arg("-c")
            .arg("printf '%s\\n' '#!/bin/sh' 'echo \"found $*\"' > \"$1\" && chmod 755 \"$1\"")
            .arg("sh")
            .arg(bin.join("shpool"))
            .status()
            .unwrap();
        assert!(status.success());
        let argv = strings(&["shpool", "list"]);
        let out = Command::new("sh")
            .arg("-c")
            .arg(remote_run_command(&argv))
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(after_start_marker(&stdout), Some("found list\n"));
    }

    #[test]
    fn every_ssh_client_shares_the_master_and_never_starts_one() {
        // Plan-ssh-recovery rule 4, and rule 15: no ControlMaster=auto and no
        // ControlPersist, so no client ever becomes a master.
        let runner = remote("user@deepthought");
        let argv = strings(&["true"]);
        let host = RemoteHost::new("user@deepthought").unwrap();
        let mut expected = strings(&[
            "ssh",
            "-S",
            "/run/user/1000/i3mux/user@deepthought.ctl",
            "-o",
            "ControlMaster=no",
            "-o",
            "ClearAllForwardings=yes",
        ]);
        expected.extend(LIVENESS_OPTIONS.map(str::to_owned));
        let run = runner.ssh_argv(&host, false, &argv);
        let attach = runner.ssh_argv(&host, true, &argv);
        assert_eq!(run[..expected.len()], expected);
        assert_eq!(attach[2..2 + expected.len()], expected);
        for argv in [&run, &attach] {
            assert!(
                !argv
                    .iter()
                    .any(|a| a.contains("ControlPersist") || a == "-M"),
                "{argv:?}"
            );
        }
    }

    /// Masters that refuse to start, and record each host they were asked for.
    #[derive(Default)]
    struct RefusingMasters {
        asked: std::sync::Mutex<Vec<RemoteHost>>,
    }

    impl Masters for RefusingMasters {
        fn ensure_started(&self, host: &RemoteHost) -> Result<master::Link, MasterError> {
            self.asked.lock().unwrap().push(host.clone());
            Err(MasterError::Systemd {
                command: "systemd-run".into(),
                detail: "refused".into(),
            })
        }
        fn stop(&self, _: &RemoteHost) -> Result<(), MasterError> {
            unreachable!()
        }
        fn state(&self, _: &RemoteHost) -> Result<master::MasterState, MasterError> {
            unreachable!()
        }
    }

    #[test]
    fn every_remote_command_starts_the_master_before_ssh() {
        // Plan-ssh-recovery rule 1. The master refuses, so ssh never runs and
        // the master's error reaches the caller.
        let masters = Arc::new(RefusingMasters::default());
        let runner = HostRunner::new(
            Host::Ssh(RemoteHost::new("deepthought").unwrap()),
            RunnerConfig {
                local_home: "/home/me".into(),
                local_runtime_dir: None,
                ssh_control_dir: "/run/user/1000/i3mux".into(),
                masters: masters.clone(),
            },
        );
        let err = runner.run(&strings(&["true"]), None).unwrap_err();
        assert!(matches!(err, HostError::Master(_)), "{err:?}");
        assert!(runner.read_file("x").is_err());
        assert_eq!(
            *masters.asked.lock().unwrap(),
            [
                RemoteHost::new("deepthought").unwrap(),
                RemoteHost::new("deepthought").unwrap()
            ]
        );
    }

    #[test]
    fn ssh_run_argv_ends_options_before_the_command() {
        let runner = remote("user@deepthought");
        let argv = runner.ssh_argv(
            &RemoteHost::new("user@deepthought").unwrap(),
            false,
            &strings(&["true"]),
        );
        assert!(argv.contains(&"-T".to_owned()));
        let host_at = argv.iter().position(|a| a == "user@deepthought").unwrap();
        assert_eq!(argv[host_at + 1], "--");
        assert_eq!(
            argv[host_at + 2],
            remote_command(&remote_script(&strings(&["true"]), true))
        );
        assert!(
            argv[host_at + 2].ends_with("; true'"),
            "{}",
            argv[host_at + 2]
        );
        assert_eq!(argv.len(), host_at + 3);
    }

    #[test]
    fn ssh_attach_argv_allocates_tty_and_sets_term() {
        let argv = remote("deepthought").attach_argv(&strings(&["shpool", "attach", "ws4-001"]));
        assert_eq!(argv[..3], ["env", "TERM=xterm-256color", "ssh"]);
        assert!(argv.contains(&"-t".to_owned()));
        let script = &shell_words::split(argv.last().unwrap()).unwrap()[2];
        assert_eq!(
            *script,
            format!("{REMOTE_PATH_PREFIX} shpool attach ws4-001")
        );
        assert!(!script.contains("i3mux-start"), "no marker on a terminal");
    }

    #[test]
    fn local_attach_argv_is_unchanged() {
        let home = tempfile::tempdir().unwrap();
        let argv = strings(&["shpool", "attach", "a b"]);
        assert_eq!(local(home.path()).attach_argv(&argv), argv);
    }

    #[test]
    fn local_run_returns_stdout_and_types_failures() {
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let out = host
            .run(&strings(&["sh", "-c", "printf %s \"$HOME\""]), None)
            .unwrap();
        assert_eq!(out, home.path().to_str().unwrap());
        let err = host
            .run(&strings(&["sh", "-c", "echo boom >&2; exit 3"]), None)
            .unwrap_err();
        assert!(
            matches!(&err, HostError::Failed { code: Some(3), stderr, .. } if stderr.trim() == "boom"),
            "{err:?}"
        );
        assert!(matches!(
            host.run(&strings(&["/nonexistent/i3mux-test"]), None),
            Err(HostError::Spawn { .. })
        ));
        assert!(matches!(host.run(&[], None), Err(HostError::EmptyCommand)));
    }

    #[test]
    fn local_stdin_reaches_the_command() {
        let home = tempfile::tempdir().unwrap();
        let out = local(home.path())
            .run(&strings(&["cat"]), Some("hello\n"))
            .unwrap();
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn file_operations_round_trip_under_home() {
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let rel = ".local/state/i3mux/sessions/ws4.json";
        assert_eq!(host.read_file(rel).unwrap(), None);
        assert_eq!(
            host.list_dir(".local/state/i3mux/sessions").unwrap(),
            Vec::<String>::new()
        );

        let content = "{\"a\": \"it's $HOME\"}\n";
        write_with_no_token(&host, rel, content);
        assert_eq!(
            std::fs::read_to_string(home.path().join(rel)).unwrap(),
            content
        );
        assert_eq!(host.read_file(rel).unwrap().as_deref(), Some(content));

        write_with_no_token(&host, ".local/state/i3mux/sessions/ws5.json", "x");
        let mut names = host.list_dir(".local/state/i3mux/sessions").unwrap();
        names.sort();
        assert_eq!(names, ["ws4.json", "ws5.json"]);

        assert!(host.remove_file(rel).unwrap());
        assert!(!host.remove_file(rel).unwrap());
        assert_eq!(host.read_file(rel).unwrap(), None);
    }

    /// Write `content` to `rel`, guarded by the token of a session that has
    /// none.
    fn write_with_no_token(host: &HostRunner, rel: &str, content: &str) {
        let written = host
            .write_file_if_token(rel, content, &ws("untaken"), None)
            .unwrap();
        assert_eq!(written, CheckedWrite::Written);
    }

    fn ws(name: &str) -> SessionName {
        SessionName::new(name).unwrap()
    }

    const LAYOUT_REL: &str = ".local/state/i3mux/sessions/ws4.json";

    /// The token directory of `local(home)`.
    fn token_dir(home: &Path) -> PathBuf {
        home.join("run/i3mux/sessions")
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn take(host: &HostRunner) -> Token {
        Token::parse(&host.take_token(&ws("ws4")).unwrap()).unwrap()
    }

    fn checked_write(host: &HostRunner, content: &str, token: Option<&Token>) -> CheckedWrite {
        host.write_file_if_token(LAYOUT_REL, content, &ws("ws4"), token)
            .unwrap()
    }

    #[test]
    fn failed_write_leaves_previous_file_and_no_temp() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let token = take(&host);
        checked_write(&host, "old", Some(&token));
        let dir = home.path().join(".local/state/i3mux/sessions");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = host.write_file_if_token(LAYOUT_REL, "new", &ws("ws4"), Some(&token));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(result, Err(HostError::Failed { .. })),
            "{result:?}"
        );
        assert_eq!(host.read_file(LAYOUT_REL).unwrap().as_deref(), Some("old"));
        assert_eq!(file_names(&dir), ["ws4.json"], "temp file left behind");
    }

    #[test]
    fn take_writes_a_new_random_token_in_the_runtime_dir() {
        // Plan-layout-backup rules 1 and 1a.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let first = take(&host);
        let second = take(&host);
        assert_ne!(first, second);
        assert_eq!(
            std::fs::read_to_string(token_dir(home.path()).join("ws4.token")).unwrap(),
            format!("{}\n", second.as_str())
        );
        assert_eq!(
            host.read_token(&ws("ws4")).unwrap(),
            Some(format!("{}\n", second.as_str()))
        );
        assert_eq!(
            file_names(&token_dir(home.path())),
            [".lock", "ws4.token"],
            "no temp file left"
        );
        assert!(
            !home.path().join(".local").exists(),
            "nothing on disk in the home directory"
        );
    }

    #[test]
    fn without_a_runtime_dir_the_token_lives_in_the_state_dir() {
        // Plan-layout-backup rule 1a.
        let home = tempfile::tempdir().unwrap();
        let state_dir = home.path().join(".local/state/i3mux/sessions");
        for runtime_dir in [None, Some(PathBuf::new())] {
            let host = local_with_runtime_dir(home.path(), runtime_dir);
            let token = take(&host);
            assert_eq!(
                std::fs::read_to_string(state_dir.join("ws4.token")).unwrap(),
                format!("{}\n", token.as_str())
            );
            assert_eq!(
                checked_write(&host, "x", Some(&token)),
                CheckedWrite::Written
            );
            host.remove_token(&ws("ws4")).unwrap();
            assert!(!state_dir.join("ws4.token").exists());
        }
    }

    #[test]
    fn take_survives_the_remote_quoting() {
        // The script has quotes and a `\n` for `tr`; both pass through
        // the login shell and `bash -lc` as on a remote host.
        let home = tempfile::tempdir().unwrap();
        let argv = script_argv(TAKE_SCRIPT, &["ws4"]);
        let printed = run_like_sshd(&remote_run_command(&argv), home.path());
        let token = Token::parse(after_start_marker(&printed).unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(token_dir(home.path()).join("ws4.token")).unwrap(),
            format!("{}\n", token.as_str())
        );
    }

    #[test]
    fn read_token_never_writes() {
        // Plan-layout-backup rule 3.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        assert_eq!(host.read_token(&ws("ws4")).unwrap(), None);
        assert!(!home.path().join("run").exists());
    }

    #[test]
    fn remove_token_deletes_only_that_token() {
        // Plan-layout-backup rule 1b.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        host.remove_token(&ws("ws4")).unwrap();
        take(&host);
        host.take_token(&ws("ws7")).unwrap();
        host.remove_token(&ws("ws4")).unwrap();
        host.remove_token(&ws("ws4")).unwrap();
        assert_eq!(host.read_token(&ws("ws4")).unwrap(), None);
        assert_eq!(file_names(&token_dir(home.path())), [".lock", "ws7.token"]);
    }

    #[test]
    fn checked_write_with_no_token_on_either_side_writes() {
        // Plan-layout-backup rule 4: bindings from before tokens still detach.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        assert_eq!(checked_write(&host, "layout", None), CheckedWrite::Written);
        assert_eq!(
            host.read_file(LAYOUT_REL).unwrap().as_deref(),
            Some("layout")
        );
        assert_eq!(
            host.read_token(&ws("ws4")).unwrap(),
            None,
            "a write never takes"
        );
    }

    #[test]
    fn checked_write_writes_only_on_the_expected_token() {
        // Plan-layout-backup rule 4.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let mine = take(&host);
        assert_eq!(
            checked_write(&host, "mine", Some(&mine)),
            CheckedWrite::Written
        );

        let theirs = take(&host);
        for stale in [Some(&mine), None] {
            assert_eq!(
                checked_write(&host, "stale", stale),
                CheckedWrite::OtherToken
            );
        }
        assert_eq!(host.read_file(LAYOUT_REL).unwrap().as_deref(), Some("mine"));

        assert_eq!(
            checked_write(&host, "theirs", Some(&theirs)),
            CheckedWrite::Written
        );
        assert_eq!(
            host.read_file(LAYOUT_REL).unwrap().as_deref(),
            Some("theirs")
        );
    }

    #[test]
    fn checked_write_with_a_token_and_no_token_file_is_an_ended_session() {
        // Plan-layout-backup rules 1b and 5.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let never = Token::parse("0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(
            checked_write(&host, "x", Some(&never)),
            CheckedWrite::NoTokenFile
        );
        let removed = take(&host);
        host.remove_token(&ws("ws4")).unwrap();
        assert_eq!(
            checked_write(&host, "x", Some(&removed)),
            CheckedWrite::NoTokenFile
        );
        assert_eq!(host.read_file(LAYOUT_REL).unwrap(), None);
    }

    #[test]
    fn refused_checked_writes_read_all_of_a_large_layout() {
        // Larger than a pipe buffer: a script that exits without reading
        // stdin would fail the write with a broken pipe.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let large = "x".repeat(1 << 20);
        let never = Token::parse("0123456789abcdef0123456789abcdef").unwrap();
        let ended = host.write_file_if_token(LAYOUT_REL, &large, &ws("ws4"), Some(&never));
        assert!(matches!(ended, Ok(CheckedWrite::NoTokenFile)), "{ended:?}");
        take(&host);
        let other = host.write_file_if_token(LAYOUT_REL, &large, &ws("ws4"), None);
        assert!(matches!(other, Ok(CheckedWrite::OtherToken)), "{other:?}");
    }

    /// A token operation on a host, run in another thread; `true` on success.
    type TokenOperation = Box<dyn FnOnce(&HostRunner) -> bool + Send>;

    #[test]
    fn token_writes_wait_for_the_lock() {
        // Plan-layout-backup rules 1, 1b and 4: take, delete and checked
        // write run under `flock`, so none can land inside another.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let token = take(&host);
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .open(token_dir(home.path()).join(".lock"))
            .unwrap();
        lock.lock().unwrap();

        let (done, finished) = std::sync::mpsc::channel();
        let operations: [TokenOperation; 3] = [
            Box::new(|h| h.take_token(&ws("ws4")).is_ok()),
            Box::new(|h| h.remove_token(&ws("ws4")).is_ok()),
            Box::new(move |h| {
                h.write_file_if_token(LAYOUT_REL, "x", &ws("ws4"), Some(&token))
                    .is_ok()
            }),
        ];
        for operation in operations {
            let (host, done) = (host.clone(), done.clone());
            std::thread::spawn(move || done.send(operation(&host)));
        }
        let wait = std::time::Duration::from_millis(300);
        assert!(finished.recv_timeout(wait).is_err(), "ran without the lock");

        drop(lock);
        let wait = std::time::Duration::from_secs(10);
        for _ in 0..3 {
            assert!(finished.recv_timeout(wait).unwrap());
        }
    }
}
