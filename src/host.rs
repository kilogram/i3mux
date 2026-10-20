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
//! - Token files (plan-layout-backup rules 1 and 4): [`HostExec::take_token`]
//!   and [`HostExec::write_file_if_token`] each run as one script under an
//!   exclusive `flock` on `.lock` in the token file's directory. So a take
//!   never lands between a checked write's compare and its write. No
//!   operation deletes a token file, and no operation writes a file without
//!   checking a token.
//! - Output of the remote login profile never reaches the caller: a remote
//!   command prints [`START_MARKER`] after the profile has run, and
//!   [`HostExec::run`] drops everything up to and including it.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::master::{self, MasterError, Masters, LIVENESS_OPTIONS};
use crate::types::{Host, RemoteHost, Token};

/// Exit status the file scripts use for "no such file" (the literal 44 in
/// `READ_SCRIPT` and `REMOVE_SCRIPT`).
const MISSING: i32 = 44;

/// Exit status of `CHECKED_WRITE_SCRIPT` when the token file does not hold
/// the expected token (the literal 45 in the script).
const OTHER_TOKEN: i32 = 45;

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

/// `$1` is the token file. Under the lock, replace it with 16 bytes from
/// `/dev/urandom` as hex, then print them.
const TAKE_SCRIPT: &str = r#"set -e
f="$HOME/$1"
d="${f%/*}"
mkdir -p "$d"
exec 9>"$d/.lock"
flock 9
new=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
if [ "${#new}" -ne 32 ]; then
  echo "i3mux: cannot read 16 random bytes from /dev/urandom" >&2
  exit 1
fi
t=$(mktemp "$d/.${f##*/}.XXXXXX")
trap 'rm -f "$t"' EXIT
printf '%s\n' "$new" > "$t"
mv -f "$t" "$f"
printf '%s' "$new""#;

/// `$1` is the file to write, `$2` the token file, `$3` the expected token
/// or empty for "no token file". Under the lock, compare, then write stdin
/// to `$1` atomically, or exit 45 and write nothing. A mismatch still reads
/// all of stdin, so the writer never sees a broken pipe.
const CHECKED_WRITE_SCRIPT: &str = r#"set -e
f="$HOME/$1"
tok="$HOME/$2"
d="${f%/*}"
mkdir -p "$d" "${tok%/*}"
exec 9>"${tok%/*}/.lock"
flock 9
found=
if [ -e "$tok" ]; then found=$(cat -- "$tok"); fi
if [ "$found" != "$3" ]; then
  cat > /dev/null
  exit 45
fi
t=$(mktemp "$d/.${f##*/}.XXXXXX")
trap 'rm -f "$t"' EXIT
cat > "$t"
mv -f "$t" "$f""#;

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

    /// Replace the token file at `token_rel` with a new token: 16 random
    /// bytes as 32 lowercase hex digits. Returns what it wrote, unvalidated.
    /// Runs under the lock of the token's directory.
    ///
    /// # Errors
    /// Any [`HostError`]; the token file is then unchanged.
    fn take_token(&self, token_rel: &str) -> Result<String, HostError>;

    /// Replace the file at `home_rel` with `content` atomically, creating
    /// parent directories, but only if the token file at `token_rel` holds
    /// `token`, or does not exist when `token` is `None`. The compare and the
    /// write run under the lock of the token's directory. Returns whether it
    /// wrote.
    ///
    /// # Errors
    /// Any [`HostError`]; the file is then unchanged.
    fn write_file_if_token(
        &self,
        home_rel: &str,
        content: &str,
        token_rel: &str,
        token: Option<&Token>,
    ) -> Result<bool, HostError>;
}

/// Local settings a [`HostRunner`] needs.
#[derive(Clone)]
pub struct RunnerConfig {
    /// Home directory of the local user; exported as `HOME` to local commands.
    pub local_home: PathBuf,
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

    fn take_token(&self, token_rel: &str) -> Result<String, HostError> {
        self.run_script(TAKE_SCRIPT, &[token_rel], None)
    }

    fn write_file_if_token(
        &self,
        home_rel: &str,
        content: &str,
        token_rel: &str,
        token: Option<&Token>,
    ) -> Result<bool, HostError> {
        // A token is never empty, so "" can stand for "no token file".
        let expected = token.map_or("", Token::as_str);
        let args = [home_rel, token_rel, expected];
        match self.run_script(CHECKED_WRITE_SCRIPT, &args, Some(content)) {
            Ok(_) => Ok(true),
            Err(HostError::Failed {
                code: Some(OTHER_TOKEN),
                ..
            }) => Ok(false),
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

    fn local(home: &Path) -> HostRunner {
        HostRunner::new(
            Host::Local,
            RunnerConfig {
                local_home: home.to_path_buf(),
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
    /// `bash -l` reads no user profile.
    fn run_like_sshd(remote_cmd: &str, home: &Path) -> String {
        let out = Command::new("sh")
            .arg("-c")
            .arg(remote_cmd)
            .env("HOME", home)
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
        assert_eq!(names, ["ws4.json", "ws5.json"], "the lock file is hidden");

        assert!(host.remove_file(rel).unwrap());
        assert!(!host.remove_file(rel).unwrap());
        assert_eq!(host.read_file(rel).unwrap(), None);
    }

    /// Write `content` to `rel`, guarded by a token file that does not exist.
    fn write_with_no_token(host: &HostRunner, rel: &str, content: &str) {
        let token_rel = format!("{rel}.token");
        assert!(host
            .write_file_if_token(rel, content, &token_rel, None)
            .unwrap());
    }

    #[test]
    fn failed_write_leaves_previous_file_and_no_temp() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let token = Token::parse(&host.take_token(TOKEN_REL).unwrap()).unwrap();
        host.write_file_if_token(LAYOUT_REL, "old", TOKEN_REL, Some(&token))
            .unwrap();
        let dir = home.path().join(".local/state/i3mux/sessions");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = host.write_file_if_token(LAYOUT_REL, "new", TOKEN_REL, Some(&token));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(result, Err(HostError::Failed { .. })),
            "{result:?}"
        );
        assert_eq!(host.read_file(LAYOUT_REL).unwrap().as_deref(), Some("old"));
        assert_eq!(
            file_names(&dir),
            [".lock", "ws4.json", "ws4.token"],
            "temp file left behind"
        );
    }

    const TOKEN_REL: &str = ".local/state/i3mux/sessions/ws4.token";
    const LAYOUT_REL: &str = ".local/state/i3mux/sessions/ws4.json";

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn take_writes_a_new_random_token_each_time() {
        // Plan-layout-backup rule 1.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let first = Token::parse(&host.take_token(TOKEN_REL).unwrap()).unwrap();
        let second = Token::parse(&host.take_token(TOKEN_REL).unwrap()).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            host.read_file(TOKEN_REL).unwrap(),
            Some(format!("{}\n", second.as_str()))
        );
        let dir = home.path().join(".local/state/i3mux/sessions");
        assert_eq!(
            file_names(&dir),
            [".lock", "ws4.token"],
            "no temp file left"
        );
    }

    #[test]
    fn take_survives_the_remote_quoting() {
        // The script has quotes and a `\n` for `tr`; both pass through
        // the login shell and `bash -lc` as on a remote host.
        let home = tempfile::tempdir().unwrap();
        let argv = script_argv(TAKE_SCRIPT, &[TOKEN_REL]);
        let printed = run_like_sshd(&remote_run_command(&argv), home.path());
        let token = Token::parse(after_start_marker(&printed).unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.path().join(TOKEN_REL)).unwrap(),
            format!("{}\n", token.as_str())
        );
    }

    #[test]
    fn checked_write_with_no_token_on_either_side_writes() {
        // Plan-layout-backup rule 4: bindings from before tokens still detach.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        assert!(host
            .write_file_if_token(LAYOUT_REL, "layout", TOKEN_REL, None)
            .unwrap());
        assert_eq!(
            host.read_file(LAYOUT_REL).unwrap().as_deref(),
            Some("layout")
        );
        assert_eq!(
            host.read_file(TOKEN_REL).unwrap(),
            None,
            "a write never takes"
        );
    }

    #[test]
    fn checked_write_writes_only_on_the_expected_token() {
        // Plan-layout-backup rule 4.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let mine = Token::parse(&host.take_token(TOKEN_REL).unwrap()).unwrap();
        assert!(host
            .write_file_if_token(LAYOUT_REL, "mine", TOKEN_REL, Some(&mine))
            .unwrap());

        let theirs = Token::parse(&host.take_token(TOKEN_REL).unwrap()).unwrap();
        for stale in [Some(&mine), None] {
            assert!(!host
                .write_file_if_token(LAYOUT_REL, "stale", TOKEN_REL, stale)
                .unwrap());
        }
        assert_eq!(host.read_file(LAYOUT_REL).unwrap().as_deref(), Some("mine"));

        assert!(host
            .write_file_if_token(LAYOUT_REL, "theirs", TOKEN_REL, Some(&theirs))
            .unwrap());
        assert_eq!(
            host.read_file(LAYOUT_REL).unwrap().as_deref(),
            Some("theirs")
        );
    }

    #[test]
    fn checked_write_with_a_token_needs_the_token_file() {
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let token = Token::parse("0123456789abcdef0123456789abcdef").unwrap();
        assert!(!host
            .write_file_if_token(LAYOUT_REL, "layout", TOKEN_REL, Some(&token))
            .unwrap());
        assert_eq!(host.read_file(LAYOUT_REL).unwrap(), None);
    }

    #[test]
    fn checked_write_mismatch_reads_all_of_a_large_layout() {
        // Larger than a pipe buffer: a script that exits without reading
        // stdin would fail the write with a broken pipe.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        host.take_token(TOKEN_REL).unwrap();
        let large = "x".repeat(1 << 20);
        let result = host.write_file_if_token(LAYOUT_REL, &large, TOKEN_REL, None);
        assert!(matches!(result, Ok(false)), "{result:?}");
    }

    #[test]
    fn take_and_checked_write_wait_for_the_lock() {
        // Plan-layout-backup rules 1 and 4: both run under `flock`, so
        // neither can land inside the other.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let token = Token::parse(&host.take_token(TOKEN_REL).unwrap()).unwrap();
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .open(home.path().join(".local/state/i3mux/sessions/.lock"))
            .unwrap();
        lock.lock().unwrap();

        let (done, finished) = std::sync::mpsc::channel();
        let (take_host, write_host) = (host.clone(), host.clone());
        let take_done = done.clone();
        std::thread::spawn(move || take_done.send(take_host.take_token(TOKEN_REL).is_ok()));
        std::thread::spawn(move || {
            let wrote = write_host.write_file_if_token(LAYOUT_REL, "x", TOKEN_REL, Some(&token));
            done.send(wrote.is_ok())
        });
        let wait = std::time::Duration::from_millis(300);
        assert!(finished.recv_timeout(wait).is_err(), "ran without the lock");

        drop(lock);
        let wait = std::time::Duration::from_secs(10);
        assert!(finished.recv_timeout(wait).unwrap());
        assert!(finished.recv_timeout(wait).unwrap());
    }
}
