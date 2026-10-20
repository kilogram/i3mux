//! The ssh master of each remote host, run as a transient systemd user unit
//! (docs/PLAN-ssh-recovery-controlmaster.md, decisions 1 and 2).
//!
//! One master per remote host holds the only network connection to it.
//! Every remote ssh that i3mux runs, for a command or for a window, is a
//! client of that master. When the link drops, systemd restarts the master,
//! so only one process per host retries the network.
//!
//! Guarantees:
//!
//! - The unit of a host is `i3mux-ssh-<host escaped by systemd-escape>.service`
//!   and its control socket is `<control dir>/<host>.ctl`. Both depend on the
//!   host alone, so every i3mux process and every window agree on them.
//! - [`Masters::ensure_started`] is idempotent and safe to run in parallel: a
//!   unit that exists already counts as started (rule 1).
//! - A master that does not start is [`MasterError::StartFailed`], which names
//!   the unit and its `journalctl` command (rule 6). systemd keeps retrying;
//!   the command's caller stops the unit if no workspace uses the host.
//! - [`TrackedMasters`] remembers every host a command asked for, so the
//!   command can stop the masters it leaves unused (rule 12).
//! - [`Masters::state`] asks systemd only. It never contacts the host (rule 13).
//! - [`Masters::stop`] of a unit that does not run is not an error.
//! - The master and every client share [`LIVENESS_OPTIONS`]: a dead link
//!   fails within about 30 s instead of hanging, and ssh never prompts.
//! - The unit is part of `graphical-session.target`, so logout stops it
//!   (rule 12).
//! - On a machine with no systemd user manager, [`DirectSsh`] stands in for
//!   [`SystemdMasters`]: there is no master, each client connects on its own,
//!   and windows do not recover.
//! - [`start_transient_unit`] is the one way i3mux starts a transient user
//!   unit; the layout watcher's unit uses it too (plan-layout-backup
//!   rule 17).

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;

use crate::types::RemoteHost;

/// ssh options of the master (rule 2) and of every client (rule 4): detect a
/// dead link after 3 missed keepalives 10 s apart, give up a connect after
/// 10 s, and never prompt.
pub const LIVENESS_OPTIONS: [&str; 8] = [
    "-o",
    "ServerAliveInterval=10",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "BatchMode=yes",
];

/// systemd properties of a master unit (rules 3 and 12). systemd restarts the
/// master forever, 2 s after a failure at first and up to 30 s apart later.
const UNIT_PROPERTIES: [&str; 8] = [
    "Type=forking",
    "Restart=always",
    "RestartSec=2s",
    "RestartSteps=5",
    "RestartMaxDelaySec=30s",
    "StartLimitIntervalSec=0",
    "PartOf=graphical-session.target",
    "After=graphical-session.target",
];

const UNIT_PREFIX: &str = "i3mux-ssh-";
const UNIT_SUFFIX: &str = ".service";

/// The control socket of the master of `host`: `<dir>/<host>.ctl`.
///
/// A [`RemoteHost`] has no `/` and no `%`, so it is a safe file name and ssh
/// does not expand it. The name stays short: a Unix socket path holds at
/// most 107 bytes, and ssh adds 17 while it creates the socket.
#[must_use]
pub fn control_socket(dir: &Path, host: &RemoteHost) -> PathBuf {
    dir.join(format!("{host}.ctl"))
}

/// Whether a systemd user manager runs for this user. It always creates the
/// socket `systemd/private` in the user's runtime directory.
#[must_use]
pub fn systemd_user_manager_runs(runtime_dir: &Path) -> bool {
    runtime_dir.join("systemd/private").exists()
}

/// The name of the systemd user unit that runs the master of one host, for
/// example `i3mux-ssh-deepthought.service`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitName(String);

impl UnitName {
    /// The unit name for a host that `systemd-escape` turned into `escaped`.
    ///
    /// # Errors
    /// [`MasterError::Systemd`] if `escaped` is empty or has a character that
    /// a unit name cannot have.
    pub fn from_escaped_host(escaped: &str) -> Result<Self, MasterError> {
        let valid = |c: char| c.is_ascii_alphanumeric() || ":_.\\-".contains(c);
        if escaped.is_empty() || !escaped.chars().all(valid) {
            return Err(MasterError::Systemd {
                command: "systemd-escape".to_owned(),
                detail: format!("unexpected output {escaped:?}"),
            });
        }
        Ok(Self(format!("{UNIT_PREFIX}{escaped}{UNIT_SUFFIX}")))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `journalctl --user -u UNIT`, quoted so that it can be pasted into a shell.
    #[must_use]
    pub fn journal_command(&self) -> String {
        format!("journalctl --user -u {}", shell_words::quote(&self.0))
    }
}

impl fmt::Display for UnitName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How the ssh clients of a remote host reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// Through the master that this unit runs.
    Shared(UnitName),
    /// Each client connects on its own: this machine runs no masters.
    Direct,
}

/// The connection state that `i3mux status` shows for a remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MasterState {
    /// This machine runs no masters; each ssh connects on its own.
    Direct,
    /// The master unit as systemd sees it.
    Unit {
        unit: UnitName,
        /// systemd's `ActiveState`: `active`, `activating`, `inactive`, ...
        active_state: String,
        /// systemd's `NRestarts`.
        restarts: u32,
    },
}

impl fmt::Display for MasterState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct => f.write_str("direct ssh (no systemd user manager)"),
            Self::Unit {
                unit,
                active_state,
                restarts,
            } => {
                let noun = if *restarts == 1 {
                    "restart"
                } else {
                    "restarts"
                };
                write!(f, "{active_state}, {restarts} {noun} ({unit})")
            }
        }
    }
}

/// A master could not be started, stopped or read.
#[derive(Debug, thiserror::Error)]
pub enum MasterError {
    #[error("the ssh connection to {host} did not start: {detail}. Log: {}", unit.journal_command())]
    StartFailed {
        host: RemoteHost,
        unit: UnitName,
        /// The last line ssh logged, or systemd-run's error.
        detail: String,
    },
    #[error("'{command}' failed: {detail}")]
    Systemd { command: String, detail: String },
    #[error("cannot start '{program}': {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// Starts, stops and reads the ssh masters of remote hosts.
/// [`SystemdMasters`] and [`DirectSsh`] are the real ones; tests use fakes.
pub trait Masters {
    /// Start the master of `host` unless it is active, and return how clients
    /// reach the host. Safe to call from several processes at once.
    ///
    /// # Errors
    /// [`MasterError::StartFailed`] if the master does not start, or another
    /// [`MasterError`] if systemd cannot be asked.
    fn ensure_started(&self, host: &RemoteHost) -> Result<Link, MasterError>;

    /// Stop the master of `host`. Its clients, windows included, lose their
    /// connection. A master that does not run is not an error.
    ///
    /// # Errors
    /// A [`MasterError`] if systemd refuses.
    fn stop(&self, host: &RemoteHost) -> Result<(), MasterError>;

    /// The master's state, from systemd only.
    ///
    /// # Errors
    /// A [`MasterError`] if systemd cannot be asked.
    fn state(&self, host: &RemoteHost) -> Result<MasterState, MasterError>;
}

/// Runs each master as a transient systemd user unit.
#[derive(Debug, Clone)]
pub struct SystemdMasters {
    control_dir: PathBuf,
    /// The caller's `SSH_AUTH_SOCK`, passed to new units.
    auth_sock: Option<String>,
}

impl SystemdMasters {
    /// Masters with their control sockets in `control_dir`, a private (0700)
    /// directory. New units get `auth_sock` as `SSH_AUTH_SOCK`.
    #[must_use]
    pub fn new(control_dir: PathBuf, auth_sock: Option<String>) -> Self {
        Self {
            control_dir,
            auth_sock,
        }
    }

    /// `systemd-run` that starts `unit`: `ssh -M -f -N` with the user's
    /// forwards, as a fresh ssh has them (rule 2). A forward that collides
    /// with another connection is only a warning.
    fn start_argv(&self, unit: &UnitName, host: &RemoteHost) -> Vec<String> {
        let mut argv: Vec<String> = ["systemd-run", "--user", "--quiet", "--collect"]
            .map(str::to_owned)
            .to_vec();
        argv.push(format!("--unit={unit}"));
        argv.push(format!("--description=i3mux ssh connection to {host}"));
        for property in UNIT_PROPERTIES {
            argv.extend(["-p".to_owned(), property.to_owned()]);
        }
        if let Some(sock) = &self.auth_sock {
            argv.push(format!("--setenv=SSH_AUTH_SOCK={sock}"));
        }
        argv.extend(["--", "ssh", "-M", "-S"].map(str::to_owned));
        argv.push(
            control_socket(&self.control_dir, host)
                .display()
                .to_string(),
        );
        argv.extend(["-f", "-N", "-o", "ControlPersist=no"].map(str::to_owned));
        argv.extend(LIVENESS_OPTIONS.map(str::to_owned));
        argv.extend(["-o", "ExitOnForwardFailure=no", host.as_str()].map(str::to_owned));
        argv
    }

    fn stop_unit(unit: &UnitName) -> Result<(), MasterError> {
        let out = output("systemctl", &["--user", "stop", unit.as_str()])?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.success() || stderr.contains("not loaded") {
            Ok(())
        } else {
            Err(MasterError::Systemd {
                command: format!("systemctl --user stop {unit}"),
                detail: stderr.trim().to_owned(),
            })
        }
    }

    /// The last line ssh logged in `unit`, if any.
    fn last_ssh_line(unit: &UnitName) -> Option<String> {
        let args = [
            "--user",
            "-u",
            unit.as_str(),
            "-t",
            "ssh",
            "-n",
            "1",
            "-o",
            "cat",
            "--no-pager",
        ];
        let out = output("journalctl", &args).ok()?;
        let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (!line.is_empty()).then_some(line)
    }
}

impl Masters for SystemdMasters {
    fn ensure_started(&self, host: &RemoteHost) -> Result<Link, MasterError> {
        let unit = unit_of(host)?;
        let is_active = output(
            "systemctl",
            &["--user", "is-active", "--quiet", unit.as_str()],
        )?;
        if is_active.status.success() {
            return Ok(Link::Shared(unit));
        }
        match start_transient_unit(&self.start_argv(&unit, host))? {
            UnitStart::Running => Ok(Link::Shared(unit)),
            UnitStart::Refused { stderr } => {
                let detail = Self::last_ssh_line(&unit).unwrap_or(stderr);
                Err(MasterError::StartFailed {
                    host: host.clone(),
                    unit,
                    detail,
                })
            }
        }
    }

    fn stop(&self, host: &RemoteHost) -> Result<(), MasterError> {
        Self::stop_unit(&unit_of(host)?)
    }

    fn state(&self, host: &RemoteHost) -> Result<MasterState, MasterError> {
        let unit = unit_of(host)?;
        let show = [
            "--user",
            "show",
            "-p",
            "ActiveState",
            "-p",
            "NRestarts",
            unit.as_str(),
        ];
        let out = checked_output("systemctl", &show)?;
        let (active_state, restarts) = parse_show(&String::from_utf8_lossy(&out.stdout));
        Ok(MasterState::Unit {
            unit,
            active_state,
            restarts,
        })
    }
}

/// Stands in for [`SystemdMasters`] on a machine with no systemd user
/// manager. There is no master: each ssh connects on its own, and windows
/// keep the behaviour they had before masters existed.
#[derive(Debug, Clone, Copy)]
pub struct DirectSsh;

impl Masters for DirectSsh {
    fn ensure_started(&self, _host: &RemoteHost) -> Result<Link, MasterError> {
        Ok(Link::Direct)
    }

    fn stop(&self, _host: &RemoteHost) -> Result<(), MasterError> {
        Ok(())
    }

    fn state(&self, _host: &RemoteHost) -> Result<MasterState, MasterError> {
        Ok(MasterState::Direct)
    }
}

/// Wraps the [`Masters`] of one i3mux command and remembers each host whose
/// master the command asked to start, whether it started, ran already, or
/// failed. When the command ends, its caller stops the masters of those
/// hosts that no workspace uses, and of no other host.
pub struct TrackedMasters {
    inner: Box<dyn Masters + Send + Sync>,
    touched: Mutex<Vec<RemoteHost>>,
}

impl TrackedMasters {
    #[must_use]
    pub fn new(inner: Box<dyn Masters + Send + Sync>) -> Self {
        Self {
            inner,
            touched: Mutex::default(),
        }
    }

    /// Each host passed to [`Masters::ensure_started`] so far, once, in order.
    #[must_use]
    pub fn touched_hosts(&self) -> Vec<RemoteHost> {
        self.touched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Masters for TrackedMasters {
    fn ensure_started(&self, host: &RemoteHost) -> Result<Link, MasterError> {
        {
            let mut touched = self
                .touched
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !touched.contains(host) {
                touched.push(host.clone());
            }
        }
        self.inner.ensure_started(host)
    }

    fn stop(&self, host: &RemoteHost) -> Result<(), MasterError> {
        self.inner.stop(host)
    }

    fn state(&self, host: &RemoteHost) -> Result<MasterState, MasterError> {
        self.inner.state(host)
    }
}

/// The unit of `host`, named by `systemd-escape` (rule 1).
fn unit_of(host: &RemoteHost) -> Result<UnitName, MasterError> {
    let out = checked_output("systemd-escape", &["--", host.as_str()])?;
    UnitName::from_escaped_host(String::from_utf8_lossy(&out.stdout).trim_end())
}

/// What [`start_transient_unit`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitStart {
    /// The unit runs: it started now, or it existed already.
    Running,
    /// `systemd-run` refused, with this error.
    Refused { stderr: String },
}

/// Run `argv`, a `systemd-run` command line that names its unit. A unit
/// that exists already counts as running: another i3mux started it first,
/// or it waits to restart. So callers in several processes may race
/// (rule 1).
///
/// # Errors
/// [`MasterError::Spawn`] if the program cannot start, and
/// [`MasterError::Systemd`] if `argv` is empty.
pub fn start_transient_unit(argv: &[String]) -> Result<UnitStart, MasterError> {
    let (program, arguments) = argv.split_first().ok_or_else(|| MasterError::Systemd {
        command: "systemd-run".to_owned(),
        detail: "i3mux bug: empty command line".to_owned(),
    })?;
    let out = output(program, arguments)?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() || already_exists(&stderr) {
        return Ok(UnitStart::Running);
    }
    Ok(UnitStart::Refused {
        stderr: stderr.trim().to_owned(),
    })
}

/// Whether `systemd-run` failed only because the unit exists: another i3mux
/// command started it first, or it waits to restart.
fn already_exists(stderr: &str) -> bool {
    stderr.contains("already loaded") || stderr.contains("already exists")
}

/// `ActiveState` and `NRestarts` from `systemctl show` output.
fn parse_show(stdout: &str) -> (String, u32) {
    let mut active_state = String::new();
    let mut restarts = 0;
    for (key, value) in stdout.lines().filter_map(|l| l.split_once('=')) {
        match key {
            "ActiveState" => value.clone_into(&mut active_state),
            "NRestarts" => restarts = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    (active_state, restarts)
}

/// Run `program` with captured output and no stdin.
fn output<S: AsRef<str>>(program: &str, args: &[S]) -> Result<Output, MasterError> {
    Command::new(program)
        .args(args.iter().map(AsRef::as_ref))
        .stdin(Stdio::null())
        .output()
        .map_err(|source| MasterError::Spawn {
            program: program.to_owned(),
            source,
        })
}

/// [`output`], with a non-zero exit as [`MasterError::Systemd`].
fn checked_output<S: AsRef<str>>(program: &str, args: &[S]) -> Result<Output, MasterError> {
    let out = output(program, args)?;
    if out.status.success() {
        return Ok(out);
    }
    let words: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    Err(MasterError::Systemd {
        command: format!("{program} {}", shell_words::join(words)),
        detail: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(text: &str) -> RemoteHost {
        RemoteHost::new(text).unwrap()
    }

    fn masters() -> SystemdMasters {
        SystemdMasters::new(
            "/run/user/1000/i3mux".into(),
            Some("/run/user/1000/ssh-agent.socket".into()),
        )
    }

    /// The value after each `-p` in `argv`.
    fn properties(argv: &[String]) -> Vec<&str> {
        argv.windows(2)
            .filter(|w| w[0] == "-p")
            .map(|w| w[1].as_str())
            .collect()
    }

    #[test]
    fn control_socket_is_the_host_name_in_the_control_dir() {
        let dir = Path::new("/run/user/1000/i3mux");
        assert_eq!(
            control_socket(dir, &host("user@deep-thought.lan")),
            Path::new("/run/user/1000/i3mux/user@deep-thought.lan.ctl")
        );
    }

    #[test]
    fn unit_name_wraps_the_escaped_host() {
        let unit = UnitName::from_escaped_host(r"user\x40deep\x2dthought.lan").unwrap();
        assert_eq!(
            unit.as_str(),
            r"i3mux-ssh-user\x40deep\x2dthought.lan.service"
        );
        assert_eq!(
            unit.journal_command(),
            r"journalctl --user -u 'i3mux-ssh-user\x40deep\x2dthought.lan.service'"
        );
        let plain = UnitName::from_escaped_host("deepthought").unwrap();
        assert_eq!(
            plain.journal_command(),
            "journalctl --user -u i3mux-ssh-deepthought.service"
        );
        for bad in ["", "a b", "a/b", "a\nb", "ü"] {
            assert!(UnitName::from_escaped_host(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn master_unit_restarts_forever_and_stops_at_logout() {
        let unit = UnitName::from_escaped_host("deepthought").unwrap();
        let argv = masters().start_argv(&unit, &host("deepthought"));
        assert_eq!(argv[..4], ["systemd-run", "--user", "--quiet", "--collect"]);
        assert!(argv.contains(&"--unit=i3mux-ssh-deepthought.service".to_owned()));
        assert_eq!(properties(&argv), UNIT_PROPERTIES);
        assert!(argv.contains(&"--setenv=SSH_AUTH_SOCK=/run/user/1000/ssh-agent.socket".to_owned()));
    }

    #[test]
    fn master_is_one_ssh_that_keeps_the_users_forwards() {
        // Rule 2: no ClearAllForwardings, and a forward collision only warns.
        let unit = UnitName::from_escaped_host("deepthought").unwrap();
        let argv = masters().start_argv(&unit, &host("deepthought"));
        let ssh_at = argv.iter().position(|a| a == "--").unwrap() + 1;
        let mut expected: Vec<&str> = vec![
            "ssh",
            "-M",
            "-S",
            "/run/user/1000/i3mux/deepthought.ctl",
            "-f",
            "-N",
            "-o",
            "ControlPersist=no",
        ];
        expected.extend(LIVENESS_OPTIONS);
        expected.extend(["-o", "ExitOnForwardFailure=no", "deepthought"]);
        assert_eq!(argv[ssh_at..], expected);
    }

    #[test]
    fn master_without_an_agent_sets_no_agent_socket() {
        let unit = UnitName::from_escaped_host("deepthought").unwrap();
        let argv =
            SystemdMasters::new("/run/i3mux".into(), None).start_argv(&unit, &host("deepthought"));
        assert!(!argv.iter().any(|a| a.starts_with("--setenv")), "{argv:?}");
    }

    #[test]
    fn a_unit_that_exists_counts_as_started() {
        // systemd 262 wording, and the older one.
        assert!(already_exists(
            "Failed to start transient service unit: Unit i3mux-ssh-x.service was already \
             loaded or has a fragment file."
        ));
        assert!(already_exists("Unit i3mux-ssh-x.service already exists."));
        assert!(!already_exists(
            "Job for i3mux-ssh-x.service failed because the control process exited with error code."
        ));
    }

    #[test]
    fn start_transient_unit_counts_an_existing_unit_as_running() {
        // Stand-ins for systemd-run: plain shells, no systemd involved.
        let sh = |script: &str| ["sh", "-c", script].map(str::to_owned).to_vec();
        assert_eq!(
            start_transient_unit(&sh("exit 0")).unwrap(),
            UnitStart::Running
        );
        let exists = sh("echo 'Unit i3mux-x.service already exists.' >&2; exit 1");
        assert_eq!(start_transient_unit(&exists).unwrap(), UnitStart::Running);
        assert_eq!(
            start_transient_unit(&sh("echo ' refused ' >&2; exit 1")).unwrap(),
            UnitStart::Refused {
                stderr: "refused".to_owned()
            }
        );
        assert!(start_transient_unit(&[]).is_err());
    }

    #[test]
    fn show_output_is_read_in_any_order() {
        assert_eq!(
            parse_show("NRestarts=3\nActiveState=activating\n"),
            ("activating".to_owned(), 3)
        );
        assert_eq!(parse_show(""), (String::new(), 0));
    }

    #[test]
    fn state_wording() {
        let unit = UnitName::from_escaped_host("deepthought").unwrap();
        let state = MasterState::Unit {
            unit,
            active_state: "active".into(),
            restarts: 1,
        };
        assert_eq!(
            state.to_string(),
            "active, 1 restart (i3mux-ssh-deepthought.service)"
        );
        assert_eq!(
            MasterState::Direct.to_string(),
            "direct ssh (no systemd user manager)"
        );
    }

    #[test]
    fn start_failure_names_the_unit_and_its_log() {
        let err = MasterError::StartFailed {
            host: host("deepthought"),
            unit: UnitName::from_escaped_host("deepthought").unwrap(),
            detail: "ssh: connect to host deepthought port 22: Network is unreachable".into(),
        };
        assert_eq!(
            err.to_string(),
            "the ssh connection to deepthought did not start: ssh: connect to host deepthought \
             port 22: Network is unreachable. Log: journalctl --user -u i3mux-ssh-deepthought.service"
        );
    }

    #[test]
    fn direct_ssh_has_no_master() {
        assert_eq!(DirectSsh.ensure_started(&host("h")).unwrap(), Link::Direct);
        DirectSsh.stop(&host("h")).unwrap();
        assert_eq!(DirectSsh.state(&host("h")).unwrap(), MasterState::Direct);
    }

    #[test]
    fn tracked_masters_remember_each_host_once() {
        let tracked = TrackedMasters::new(Box::new(DirectSsh));
        assert!(tracked.touched_hosts().is_empty());
        assert_eq!(tracked.ensure_started(&host("a")).unwrap(), Link::Direct);
        tracked.ensure_started(&host("b")).unwrap();
        tracked.ensure_started(&host("a")).unwrap();
        tracked.stop(&host("c")).unwrap();
        tracked.state(&host("d")).unwrap();
        assert_eq!(tracked.touched_hosts(), [host("a"), host("b")]);
    }

    #[test]
    fn user_manager_is_found_by_its_private_socket() {
        let runtime = tempfile::tempdir().unwrap();
        assert!(!systemd_user_manager_runs(runtime.path()));
        std::fs::create_dir(runtime.path().join("systemd")).unwrap();
        std::fs::write(runtime.path().join("systemd/private"), "").unwrap();
        assert!(systemd_user_manager_runs(runtime.path()));
    }
}
