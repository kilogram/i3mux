//! shpool commands on a host, and the pure rule that picks which shell a new
//! window shows.
//!
//! i3mux runs its own shpool daemon on each host, with its own socket
//! (`~/.local/run/i3mux/shpool.socket`) and config
//! (`~/.config/i3mux/shpool.toml`), so the restore mode is under i3mux
//! control and a user's own shpool daemon is never touched.
//!
//! Guarantees:
//!
//! - Every shpool command runs through [`PRELUDE`], which creates the socket
//!   directory (mode 0700) and writes the config if it is absent. No caller
//!   has to prepare a host first, and a user's config edits are never
//!   overwritten.
//! - i3mux starts the daemon itself, never shpool: every command passes
//!   `--no-daemonize`. Its output is appended to
//!   `~/.local/state/i3mux/shpool.log`, which keeps panics and survives a
//!   restart.
//! - A dead daemon costs one command: it fails with
//!   [`MuxError::DaemonRestarted`] after a new daemon has started, so the
//!   next command works.
//! - Read-only and destructive commands never start a daemon on a host
//!   with no socket. Only `attach` does.
//! - [`list`] reports a host with no daemon as having no shells, and an
//!   unreachable host as an error (plan rule 11).
//! - [`choose_terminal`] is pure (plan rule 4).

use serde::Deserialize;

use crate::host::{HostError, HostExec};
use crate::types::{Host, SessionName, TerminalName};

/// The shpool config i3mux writes when none exists.
const CONFIG: &str = r#"# Written by i3mux for its own shpool daemon. i3mux never overwrites this file.
# Replay up to 10000 lines of output when a terminal reattaches.
session_restore_mode = { lines = 10000 }
output_spool_lines = 10000
# Leave the shell prompt unchanged.
prompt_prefix = ""
# New shells see the ssh login they started from, as in a fresh ssh login.
# shpool adds these to its defaults: TERM, DISPLAY, LANG and SSH_AUTH_SOCK.
forward_env = ["SSH_CONNECTION", "SSH_CLIENT"]
"#;

/// Shell prologue of every shpool command. `$1` is the config text; the
/// remaining arguments go to shpool unparsed, after `--no-daemonize`.
///
/// `up` succeeds only if a daemon answers. Against a socket file with no
/// daemon behind it, shpool exits 1 and prints nothing, so a socket file
/// that does not answer is a dead daemon. The script then starts a new
/// daemon and exits [`DAEMON_RESTARTED`]. Before `attach` with no socket, it
/// starts a daemon silently. The new daemon itself removes the dead socket,
/// only after a connect is refused, so starting one never harms a live
/// daemon. `flock` stops parallel commands from starting two daemons.
///
/// The daemon's stderr is appended to the log: shpool's autostart would
/// truncate its log and send panics to /dev/null.
const PRELUDE: &str = r#"set -e
run_dir="$HOME/.local/run/i3mux"
config="$HOME/.config/i3mux/shpool.toml"
log="$HOME/.local/state/i3mux/shpool.log"
socket="$run_dir/shpool.socket"
mkdir -p "$run_dir" "${config%/*}" "${log%/*}"
chmod 700 "$run_dir"
if [ ! -e "$config" ]; then
  tmp="$config.$$.tmp"
  printf '%s' "$1" > "$tmp"
  ln "$tmp" "$config" 2>/dev/null || [ -e "$config" ] || mv "$tmp" "$config"
  rm -f "$tmp"
fi
shift
command -v shpool >/dev/null || exit 127
up() { shpool --socket "$socket" --config-file "$config" --no-daemonize list >/dev/null 2>&1; }
dead=
if ! up; then
  exec 9>"$run_dir/start.lock"
  flock 9
  if ! up && { [ -e "$socket" ] || [ "$1" = attach ]; }; then
    if [ -e "$socket" ]; then dead=1; fi
    if [ -f "$log" ] && [ "$(wc -c < "$log")" -gt 10485760 ]; then mv -f "$log" "$log.1"; fi
    printf '%s i3mux: %sstarting a shpool daemon\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      "${dead:+the daemon at $socket does not answer; }" >> "$log"
    setsid shpool --socket "$socket" --config-file "$config" daemon \
      < /dev/null >> "$log" 2>&1 9>&- &
    tries=0
    until up; do
      tries=$((tries + 1))
      if [ "$tries" -gt 50 ]; then
        echo "i3mux: the shpool daemon did not start; see $log" >&2
        exit 1
      fi
      sleep 0.1
    done
  fi
  exec 9>&-
fi
if [ -n "$dead" ]; then
  echo "i3mux: the shpool daemon had stopped and its shells are gone; a new one is running. Log: $log" >&2
  exit 75
fi
exec shpool --socket "$socket" --config-file "$config" --no-daemonize "$@""#;

/// What shpool prints when no daemon listens on the socket.
const NO_DAEMON: &str = "could not connect to daemon";

/// [`PRELUDE`] exits with this after it replaced a dead daemon (the literal
/// 75 in the script).
const DAEMON_RESTARTED: i32 = 75;

/// What `shpool kill` and `shpool detach` print for a name that is gone.
const NOT_FOUND: &str = "not found:";

/// What `shpool detach` prints for a shell with no client.
const NOT_ATTACHED: &str = "not attached:";

/// `sh` reports a missing program with exit status 127.
const COMMAND_NOT_FOUND: i32 = 127;

/// A shpool command failed in a way i3mux understands.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("shpool is not installed on {host}, or is not on its login PATH; see INSTALL.md")]
    NotInstalled { host: Host },
    #[error(
        "the shpool daemon on {host} had stopped, so its shells are gone. i3mux started \
         a new daemon: run the command again. Daemon log: ~/.local/state/i3mux/shpool.log on {host}"
    )]
    DaemonRestarted { host: Host },
    #[error("cannot read the output of 'shpool list' on {host}: {detail}")]
    BadList { host: Host, detail: String },
    #[error(transparent)]
    Host(#[from] HostError),
}

/// One live shell on a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    name: TerminalName,
    attached: bool,
}

impl Shell {
    fn new(name: TerminalName, attached: bool) -> Self {
        Self { name, attached }
    }

    #[must_use]
    pub fn name(&self) -> &TerminalName {
        &self.name
    }

    /// Whether some terminal (here or on another machine) shows this shell.
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.attached
    }
}

/// The argv that runs `shpool <shpool_args>` on a host through [`PRELUDE`].
fn shpool_argv(shpool_args: &[&str]) -> Vec<String> {
    let mut argv: Vec<String> = ["sh", "-c", PRELUDE, "i3mux-shpool", CONFIG]
        .map(str::to_owned)
        .to_vec();
    argv.extend(shpool_args.iter().map(|a| (*a).to_owned()));
    argv
}

/// Map "program not found" to [`MuxError::NotInstalled`] and a replaced
/// daemon to [`MuxError::DaemonRestarted`].
fn classify(host: &Host, err: HostError) -> MuxError {
    match err {
        HostError::Failed {
            code: Some(COMMAND_NOT_FOUND),
            ..
        } => MuxError::NotInstalled { host: host.clone() },
        HostError::Failed {
            code: Some(DAEMON_RESTARTED),
            ..
        } => MuxError::DaemonRestarted { host: host.clone() },
        other => MuxError::Host(other),
    }
}

fn is_no_daemon(err: &HostError) -> bool {
    matches!(err, HostError::Failed { stderr, .. } if stderr.contains(NO_DAEMON))
}

/// All i3mux shells on the host. Names that are not terminal names are
/// ignored. A host with no daemon running has no shells.
///
/// # Errors
/// [`MuxError::NotInstalled`] if shpool is missing,
/// [`MuxError::DaemonRestarted`] if the daemon was dead, [`MuxError::Host`]
/// if the host is unreachable or shpool fails, [`MuxError::BadList`] if its
/// output is not the expected JSON.
pub fn list(host: &dyn HostExec) -> Result<Vec<Shell>, MuxError> {
    let argv = shpool_argv(&["list", "--json"]);
    match host.run(&argv, None) {
        Ok(stdout) => parse_list(&stdout).map_err(|detail| MuxError::BadList {
            host: host.host().clone(),
            detail,
        }),
        Err(err) if is_no_daemon(&err) => Ok(Vec::new()),
        Err(err) => Err(classify(host.host(), err)),
    }
}

#[derive(Deserialize)]
struct ListJson {
    sessions: Vec<SessionJson>,
}

#[derive(Deserialize)]
struct SessionJson {
    name: String,
    status: String,
}

/// Parse `shpool list --json`. Any status other than `Disconnected` counts
/// as attached, so an unknown status never makes a shell look free.
fn parse_list(stdout: &str) -> Result<Vec<Shell>, String> {
    let parsed: ListJson = serde_json::from_str(stdout).map_err(|e| e.to_string())?;
    Ok(parsed
        .sessions
        .into_iter()
        .filter_map(|s| {
            let name = s.name.parse().ok()?;
            Some(Shell::new(name, s.status != "Disconnected"))
        })
        .collect())
}

/// The argv a local terminal runs to show shell `name` on `host`. The shell
/// is created if it does not exist; `cmd` (split by shpool with shell-words
/// rules) replaces the login shell of a new shell only. With `force`, any
/// other client attached to the shell is disconnected.
#[must_use]
pub fn attach_argv(
    host: &dyn HostExec,
    name: &TerminalName,
    force: bool,
    cmd: Option<&str>,
) -> Vec<String> {
    let name = name.to_string();
    let mut args = vec!["attach"];
    if force {
        args.push("-f");
    }
    if let Some(cmd) = cmd {
        args.extend(["-c", cmd]);
    }
    args.push(&name);
    host.attach_argv(&shpool_argv(&args))
}

/// Kill the given shells. Shells that are already gone are not an error.
///
/// # Errors
/// [`MuxError::NotInstalled`] or [`MuxError::Host`] if the kill fails.
pub fn kill(host: &dyn HostExec, names: &[TerminalName]) -> Result<(), MuxError> {
    if names.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = names.iter().map(ToString::to_string).collect();
    let mut args = vec!["kill"];
    args.extend(names.iter().map(String::as_str));
    match host.run(&shpool_argv(&args), None) {
        Ok(_) => Ok(()),
        Err(err) if is_no_daemon(&err) || only_misses(&err, &[NOT_FOUND]) => Ok(()),
        Err(err) => Err(classify(host.host(), err)),
    }
}

/// Disconnect every client attached to the given shells; the shells keep
/// running. Shells with no client, shells that are gone, and a host with no
/// daemon are not an error, so this is idempotent.
///
/// shpool reports a shell as attached for a short time after this returns;
/// callers that need it disconnected poll [`list`].
///
/// # Errors
/// [`MuxError::NotInstalled`] or [`MuxError::Host`] if the detach fails.
pub fn detach(host: &dyn HostExec, names: &[TerminalName]) -> Result<(), MuxError> {
    if names.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = names.iter().map(ToString::to_string).collect();
    let mut args = vec!["detach"];
    args.extend(names.iter().map(String::as_str));
    match host.run(&shpool_argv(&args), None) {
        Ok(_) => Ok(()),
        Err(err) if is_no_daemon(&err) || only_misses(&err, &[NOT_FOUND, NOT_ATTACHED]) => Ok(()),
        Err(err) => Err(classify(host.host(), err)),
    }
}

/// Whether `err` is exit 1 with only "nothing to do" lines on stderr.
fn only_misses(err: &HostError, prefixes: &[&str]) -> bool {
    matches!(err, HostError::Failed { code: Some(1), stderr, .. }
        if stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .all(|l| prefixes.iter().any(|p| l.starts_with(p))))
}

/// Kill every shell of `session`; return the names killed.
///
/// # Errors
/// As [`list`] and [`kill`].
pub fn kill_session(
    host: &dyn HostExec,
    session: &SessionName,
) -> Result<Vec<TerminalName>, MuxError> {
    let names: Vec<TerminalName> = list(host)?
        .into_iter()
        .map(|s| s.name)
        .filter(|n| n.session() == session)
        .collect();
    kill(host, &names)?;
    Ok(names)
}

/// Which shell a new window shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Show an existing shell that has no window.
    Reattach(TerminalName),
    /// Start a new shell.
    Create(TerminalName),
}

impl Choice {
    #[must_use]
    pub fn name(&self) -> &TerminalName {
        match self {
            Self::Reattach(name) | Self::Create(name) => name,
        }
    }
}

/// Pick the shell for a new window of `session` (plan rule 4, decision 5).
///
/// `alive` are the session's live shells. `in_use` are shells that must not
/// be reattached: those with a window here, and those attached elsewhere
/// unless the caller takes them over. Unless `force_new`, the lowest live
/// shell that is not in use is reattached. Otherwise a new shell gets one
/// more than the highest number in `alive` or `in_use` (001 if both are
/// empty), so a name is never reused while a window or shell still has it.
#[must_use]
pub fn choose_terminal(
    session: &SessionName,
    alive: &[TerminalName],
    in_use: &[TerminalName],
    force_new: bool,
) -> Choice {
    if !force_new {
        if let Some(orphan) = alive.iter().filter(|n| !in_use.contains(n)).min() {
            return Choice::Reattach(orphan.clone());
        }
    }
    let taken: Vec<u32> = alive
        .iter()
        .chain(in_use)
        .map(TerminalName::number)
        .collect();
    let next = match taken.iter().max() {
        None => 1,
        Some(&max) => max.checked_add(1).unwrap_or_else(|| lowest_free(&taken)),
    };
    Choice::Create(TerminalName::new(session, next))
}

/// The lowest number from 1 up that is not in `taken`.
fn lowest_free(taken: &[u32]) -> u32 {
    (1..=u32::MAX).find(|n| !taken.contains(n)).unwrap_or(0)
}

/// Real-shpool tests over a pty; ignored by default, run by `just check-mux`.
#[cfg(test)]
#[path = "mux_pty_tests.rs"]
mod pty_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RemoteHost;
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Output};

    fn ws4() -> SessionName {
        SessionName::new("ws4").unwrap()
    }

    fn t(n: u32) -> TerminalName {
        TerminalName::new(&ws4(), n)
    }

    /// A host whose `run` returns a scripted result and records the argv.
    struct ScriptedHost {
        host: Host,
        result: RefCell<Option<Result<String, HostError>>>,
        argv: RefCell<Vec<String>>,
    }

    impl ScriptedHost {
        fn new(result: Result<String, HostError>) -> Self {
            Self {
                host: Host::Ssh(RemoteHost::new("deepthought").unwrap()),
                result: RefCell::new(Some(result)),
                argv: RefCell::new(Vec::new()),
            }
        }
    }

    impl HostExec for ScriptedHost {
        fn host(&self) -> &Host {
            &self.host
        }
        fn run(&self, argv: &[String], _stdin: Option<&str>) -> Result<String, HostError> {
            *self.argv.borrow_mut() = argv.to_vec();
            self.result.borrow_mut().take().unwrap()
        }
        fn attach_argv(&self, argv: &[String]) -> Vec<String> {
            let mut out = vec!["remote:".to_owned()];
            out.extend_from_slice(argv);
            out
        }
        fn read_file(&self, _: &str) -> Result<Option<String>, HostError> {
            unreachable!()
        }
        fn list_dir(&self, _: &str) -> Result<Vec<String>, HostError> {
            unreachable!()
        }
        fn remove_file(&self, _: &str) -> Result<bool, HostError> {
            unreachable!()
        }
        fn take_token(&self, _: &SessionName) -> Result<String, HostError> {
            unreachable!()
        }
        fn read_token(&self, _: &SessionName) -> Result<Option<String>, HostError> {
            unreachable!()
        }
        fn remove_token(&self, _: &SessionName) -> Result<(), HostError> {
            unreachable!()
        }
        fn write_file_if_token(
            &self,
            _: &str,
            _: &str,
            _: &SessionName,
            _: Option<&crate::types::Token>,
        ) -> Result<crate::host::CheckedWrite, HostError> {
            unreachable!()
        }
    }

    fn failed(code: i32, stderr: &str) -> HostError {
        HostError::Failed {
            host: Host::Local,
            code: Some(code),
            stderr: stderr.to_owned(),
            command: String::new(),
        }
    }

    // ---------- choose_terminal (plan rule 4) ----------

    #[test]
    fn empty_session_creates_001() {
        assert_eq!(
            choose_terminal(&ws4(), &[], &[], false),
            Choice::Create(t(1))
        );
    }

    #[test]
    fn lowest_orphan_is_reattached() {
        let alive = [t(3), t(1), t(2)];
        assert_eq!(
            choose_terminal(&ws4(), &alive, &[t(1)], false),
            Choice::Reattach(t(2))
        );
    }

    #[test]
    fn no_orphan_creates_max_plus_one() {
        let alive = [t(1), t(2)];
        assert_eq!(
            choose_terminal(&ws4(), &alive, &alive, false),
            Choice::Create(t(3))
        );
    }

    #[test]
    fn force_new_skips_orphans() {
        let alive = [t(1), t(5)];
        assert_eq!(
            choose_terminal(&ws4(), &alive, &[], true),
            Choice::Create(t(6))
        );
    }

    #[test]
    fn windows_without_live_shells_still_reserve_their_number() {
        // A window still shows 007 (its shell is gone): never reuse 007.
        assert_eq!(
            choose_terminal(&ws4(), &[t(2)], &[t(2), t(7)], false),
            Choice::Create(t(8))
        );
    }

    #[test]
    fn number_overflow_falls_back_to_lowest_free() {
        let alive = [t(1), t(u32::MAX)];
        assert_eq!(
            choose_terminal(&ws4(), &alive, &alive, true),
            Choice::Create(t(2))
        );
    }

    proptest::proptest! {
        #[test]
        fn chosen_terminal_is_never_in_use(
            alive in proptest::collection::btree_set(1u32..40, 0..12),
            in_use in proptest::collection::btree_set(1u32..40, 0..12),
            force_new in proptest::bool::ANY,
        ) {
            let alive: Vec<TerminalName> = alive.into_iter().map(t).collect();
            let in_use: Vec<TerminalName> = in_use.into_iter().map(t).collect();
            let orphans: Vec<&TerminalName> = alive.iter().filter(|n| !in_use.contains(n)).collect();
            match choose_terminal(&ws4(), &alive, &in_use, force_new) {
                Choice::Reattach(name) => {
                    proptest::prop_assert!(!force_new);
                    proptest::prop_assert_eq!(Some(&&name), orphans.iter().min());
                }
                Choice::Create(name) => {
                    proptest::prop_assert!(force_new || orphans.is_empty());
                    proptest::prop_assert!(!alive.contains(&name) && !in_use.contains(&name));
                    proptest::prop_assert!(alive.iter().chain(&in_use).all(|n| n < &name));
                }
            }
        }
    }

    // ---------- list ----------

    const LIST_JSON: &str = r#"{"sessions": [
        {"name": "ws4-001", "status": "Attached", "shell_pid": 1},
        {"name": "ws4-002", "status": "Disconnected"},
        {"name": "not-i3mux", "status": "Disconnected"},
        {"name": "ws4-x-003", "status": "Disconnected"}
    ]}"#;

    #[test]
    fn list_parses_json_and_ignores_foreign_names() {
        let host = ScriptedHost::new(Ok(LIST_JSON.to_owned()));
        let shells = list(&host).unwrap();
        let ws4_x = SessionName::new("ws4-x").unwrap();
        assert_eq!(
            shells,
            [
                Shell::new(t(1), true),
                Shell::new(t(2), false),
                Shell::new(TerminalName::new(&ws4_x, 3), false),
            ]
        );
        let argv = host.argv.borrow();
        assert_eq!(argv[argv.len() - 2..], ["list", "--json"]);
    }

    #[test]
    fn list_with_no_daemon_is_empty() {
        let host = ScriptedHost::new(Err(failed(1, "could not connect to daemon\n")));
        assert_eq!(list(&host).unwrap(), []);
    }

    #[test]
    fn list_on_unreachable_host_is_an_error() {
        let host = ScriptedHost::new(Err(HostError::Unreachable {
            host: RemoteHost::new("deepthought").unwrap(),
            detail: "Connection refused".into(),
        }));
        assert!(matches!(
            list(&host),
            Err(MuxError::Host(HostError::Unreachable { .. }))
        ));
    }

    #[test]
    fn missing_shpool_is_reported_as_not_installed() {
        let host = ScriptedHost::new(Err(failed(127, "sh: exec: shpool: not found")));
        let err = list(&host).unwrap_err();
        assert!(matches!(err, MuxError::NotInstalled { .. }));
        assert!(err
            .to_string()
            .contains("shpool is not installed on deepthought"));
    }

    #[test]
    fn replaced_daemon_is_reported_with_the_log_path() {
        let host = ScriptedHost::new(Err(failed(DAEMON_RESTARTED, "")));
        let err = list(&host).unwrap_err();
        assert!(matches!(err, MuxError::DaemonRestarted { .. }));
        let text = err.to_string();
        assert!(text.contains("run the command again"), "{text}");
        assert!(text.contains("~/.local/state/i3mux/shpool.log on deepthought"));
    }

    #[test]
    fn garbage_list_output_is_an_error() {
        let host = ScriptedHost::new(Ok("NAME\tSTATUS\n".into()));
        assert!(matches!(list(&host), Err(MuxError::BadList { .. })));
    }

    // ---------- kill ----------

    #[test]
    fn kill_tolerates_names_already_gone() {
        let host = ScriptedHost::new(Err(failed(1, "not found: ws4-002\n")));
        kill(&host, &[t(1), t(2)]).unwrap();
        let argv = host.argv.borrow();
        assert_eq!(argv[argv.len() - 3..], ["kill", "ws4-001", "ws4-002"]);
    }

    #[test]
    fn detach_names_every_shell_in_one_call() {
        let host = ScriptedHost::new(Ok(String::new()));
        detach(&host, &[t(1), t(2)]).unwrap();
        let argv = host.argv.borrow();
        assert_eq!(argv[argv.len() - 3..], ["detach", "ws4-001", "ws4-002"]);
    }

    #[test]
    fn detach_is_idempotent() {
        // Measured with shpool 0.11.5: exit 1 with these lines.
        for stderr in [
            "not attached: ws4-001\n",
            "not found: ws4-002\n",
            "not attached: ws4-001\nnot found: ws4-002\n",
            "could not connect to daemon\n",
        ] {
            let host = ScriptedHost::new(Err(failed(1, stderr)));
            detach(&host, &[t(1), t(2)]).unwrap();
        }
        let host = ScriptedHost::new(Err(failed(1, "permission denied\n")));
        assert!(matches!(detach(&host, &[t(1)]), Err(MuxError::Host(_))));
    }

    #[test]
    fn kill_reports_other_failures() {
        let host = ScriptedHost::new(Err(failed(1, "permission denied\n")));
        assert!(matches!(kill(&host, &[t(1)]), Err(MuxError::Host(_))));
    }

    #[test]
    fn kill_of_nothing_runs_nothing() {
        let host = ScriptedHost::new(Ok(String::new()));
        kill(&host, &[]).unwrap();
        assert!(host.argv.borrow().is_empty());
    }

    // ---------- attach argv and the prelude ----------

    #[test]
    fn attach_argv_passes_force_and_command_as_separate_args() {
        let host = ScriptedHost::new(Ok(String::new()));
        let argv = attach_argv(&host, &t(2), true, Some("vim 'my file'"));
        assert_eq!(argv[0], "remote:");
        assert_eq!(
            argv[argv.len() - 5..],
            ["attach", "-f", "-c", "vim 'my file'", "ws4-002"]
        );
        let plain = attach_argv(&host, &t(2), false, None);
        assert_eq!(plain[plain.len() - 2..], ["attach", "ws4-002"]);
    }

    #[test]
    fn every_shpool_argv_creates_the_socket_dir_first() {
        // Coordinator finding: shpool does not create the socket directory.
        let argv = shpool_argv(&["list"]);
        assert_eq!(argv[..2], ["sh", "-c"]);
        let script = &argv[2];
        let mkdir = script.find("mkdir -p \"$run_dir\"").unwrap();
        let exec = script.find("exec shpool --socket \"$socket\"").unwrap();
        assert!(mkdir < exec);
        assert!(script.contains("run_dir=\"$HOME/.local/run/i3mux\""));
        assert!(script.contains("chmod 700 \"$run_dir\""));
    }

    /// A fake `shpool` that prints its arguments, one per line. As a daemon
    /// probe it answers, so the prelude sees a live daemon.
    const ECHO_SHPOOL: &str = r#"#!/bin/sh
for a in "$@"; do printf "%s\n" "$a"; done
"#;

    /// A fake `shpool` with a daemon. `$sock.alive` marks a live daemon; a
    /// socket file without it is a dead one, which `list` meets with exit 1
    /// and no output, as shpool 0.11.5 does. `daemon` records each start in
    /// `~/daemon-starts`.
    const DAEMON_SHPOOL: &str = r#"#!/bin/sh
sock=$2
shift 4
if [ "$1" = --no-daemonize ]; then shift; fi
case $1 in
list)
  if [ -e "$sock.alive" ]; then echo '{"sessions": []}'; exit 0; fi
  if [ -e "$sock" ]; then exit 1; fi
  echo "could not connect to daemon" >&2
  exit 1 ;;
daemon)
  echo started >> "$HOME/daemon-starts"
  echo "fake daemon output"
  : > "$sock"
  : > "$sock.alive" ;;
*) printf "%s\n" "$@" ;;
esac
"#;

    /// Run a prelude argv locally with `HOME` in a temp dir and `fake` as
    /// `shpool` on `PATH`.
    fn run_prelude_with(home: &Path, fake: &str, shpool_args: &[&str]) -> Output {
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // Write the fake from a child process: a write descriptor held by
        // this (multi-threaded) test process could leak into another test's
        // fork and make exec fail with ETXTBSY.
        let status = Command::new("sh")
            .arg("-c")
            .arg("printf '%s' \"$2\" > \"$1\" && chmod 755 \"$1\"")
            .arg("sh")
            .arg(bin.join("shpool"))
            .arg(fake)
            .status()
            .unwrap();
        assert!(status.success());
        let argv = shpool_argv(shpool_args);
        Command::new(&argv[0])
            .args(&argv[1..])
            .env("HOME", home)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .output()
            .unwrap()
    }

    /// [`run_prelude_with`] the echo fake; the prelude must succeed.
    fn run_prelude(home: &Path, shpool_args: &[&str]) -> String {
        let out = run_prelude_with(home, ECHO_SHPOOL, shpool_args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn daemon_starts(home: &Path) -> usize {
        std::fs::read_to_string(home.join("daemon-starts")).map_or(0, |s| s.lines().count())
    }

    fn daemon_log(home: &Path) -> String {
        std::fs::read_to_string(home.join(".local/state/i3mux/shpool.log")).unwrap_or_default()
    }

    #[test]
    fn dead_daemon_fails_one_command_and_the_next_one_works() {
        let home = tempfile::tempdir().unwrap();
        let run_dir = home.path().join(".local/run/i3mux");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("shpool.socket"), "").unwrap();

        let first = run_prelude_with(home.path(), DAEMON_SHPOOL, &["list", "--json"]);
        assert_eq!(first.status.code(), Some(DAEMON_RESTARTED), "{first:?}");
        assert!(String::from_utf8_lossy(&first.stderr).contains("had stopped"));
        assert_eq!(daemon_starts(home.path()), 1);
        let log = daemon_log(home.path());
        assert!(
            log.contains("does not answer; starting a shpool daemon"),
            "{log}"
        );
        assert!(log.contains("fake daemon output"), "{log}");

        let second = run_prelude_with(home.path(), DAEMON_SHPOOL, &["list", "--json"]);
        assert!(second.status.success(), "{second:?}");
        assert_eq!(
            String::from_utf8_lossy(&second.stdout),
            "{\"sessions\": []}\n"
        );
        assert_eq!(daemon_starts(home.path()), 1);
    }

    #[test]
    fn list_without_a_socket_starts_no_daemon() {
        let home = tempfile::tempdir().unwrap();
        let out = run_prelude_with(home.path(), DAEMON_SHPOOL, &["list", "--json"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains(NO_DAEMON));
        assert_eq!(daemon_starts(home.path()), 0);
    }

    #[test]
    fn attach_without_a_socket_starts_a_daemon_silently() {
        let home = tempfile::tempdir().unwrap();
        let out = run_prelude_with(home.path(), DAEMON_SHPOOL, &["attach", "ws4-001"]);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "attach\nws4-001\n");
        assert_eq!(daemon_starts(home.path()), 1);
        let log = daemon_log(home.path());
        assert!(log.contains("i3mux: starting a shpool daemon"), "{log}");
    }

    #[test]
    fn a_large_daemon_log_is_rotated_before_a_start() {
        let home = tempfile::tempdir().unwrap();
        let log = home.path().join(".local/state/i3mux/shpool.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, vec![b'x'; 10 * 1024 * 1024 + 1]).unwrap();
        let out = run_prelude_with(home.path(), DAEMON_SHPOOL, &["attach", "ws4-001"]);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            std::fs::metadata(log.with_extension("log.1"))
                .unwrap()
                .len(),
            10 * 1024 * 1024 + 1
        );
        assert!(daemon_log(home.path()).contains("starting a shpool daemon"));
    }

    #[test]
    fn prelude_prepares_host_and_passes_args_unparsed() {
        let home = tempfile::tempdir().unwrap();
        let out = run_prelude(
            home.path(),
            &["attach", "-c", "echo 'a b' $HOME", "ws4-001"],
        );
        let socket = home.path().join(".local/run/i3mux/shpool.socket");
        let config = home.path().join(".config/i3mux/shpool.toml");
        let expected = [
            "--socket",
            socket.to_str().unwrap(),
            "--config-file",
            config.to_str().unwrap(),
            "--no-daemonize",
            "attach",
            "-c",
            "echo 'a b' $HOME",
            "ws4-001",
        ];
        assert_eq!(out.lines().collect::<Vec<_>>(), expected);
        let run_dir = home.path().join(".local/run/i3mux");
        let mode = std::fs::metadata(&run_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), CONFIG);
    }

    #[test]
    fn prelude_never_overwrites_an_existing_config() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".config/i3mux/shpool.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "# my edits\n").unwrap();
        run_prelude(home.path(), &["list"]);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "# my edits\n");
        let leftovers: Vec<_> = std::fs::read_dir(config.parent().unwrap())
            .unwrap()
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    }

    #[test]
    fn new_shells_see_the_ssh_login_they_started_from() {
        // Plan-ssh-recovery rule 16. The key is shpool 0.11.5's spelling
        // (libshpool src/config.rs, `forward_env`).
        let config: toml::Table = toml::from_str(CONFIG).unwrap();
        assert_eq!(
            config["forward_env"],
            toml::Value::Array(vec!["SSH_CONNECTION".into(), "SSH_CLIENT".into()])
        );
    }
}
