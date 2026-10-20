//! i3mux: each Sway workspace is one "tmux window" of persistent shells.
//!
//! This file is the command line and the command handlers. Each handler
//! resolves its input at the edge, then runs a core function over [`Ports`]:
//! the state store, Sway, the terminal launcher, and a way to reach hosts.
//! Tests run the core functions against fakes.
//!
//! Every core function that changes bindings, windows or shells holds the
//! state lock from its first read to its last write, so concurrent i3mux
//! commands never interleave (plan rule 4).
//!
//! `activate`, `attach` and `kill` take their session on its host: they
//! write a new token there (plan-layout-backup rule 1). A command takes only
//! after all its refusals, under the state lock, so a refused command never
//! takes a session from the machine that holds it (rule 2). `detach` saves
//! only while the host still has its binding's token (rules 4 and 5), and
//! `kill` ends the session by deleting the token (rule 1b).
//!
//! A binding whose windows went away with an earlier Sway is resumable:
//! `attach` and `terminal` bring its windows back from this machine's
//! backup, without a take (rules 3 and 11-15). The layout watcher keeps
//! those backups: every command that makes this Sway the owner of a binding
//! starts it (rule 17).
//!
//! Every command, on success and on failure, ends in
//! [`with_unused_masters_stopped`]: it stops the ssh master of each host the
//! command touched that no workspace is bound to (plan-ssh-recovery
//! rule 12). Nothing runs on a host after that stop.

mod backup;
mod host;
mod launch;
mod layout;
mod master;
mod mux;
mod session;
mod state;
mod sway_ops;
mod types;
mod watch;
mod watch_unit;
mod window;
mod wm;

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use backup::BackupStore;
use host::{HostExec, HostRunner, RunnerConfig};
use launch::{Launcher, RemoteAttach, TerminalLauncher, WindowCommand};
use layout::{Layout, Step};
use master::{DirectSsh, Link, MasterState, Masters, SystemdMasters, TrackedMasters};
use mux::{Choice, Shell};
use serde_json::Value;
use session::{SavedSession, SessionError, TokenMatch};
use state::{BindError, LocalState, StateLock, StateStore, WorkspaceState};
use sway_ops::Sway;
use types::{Host, InvalidName, RemoteHost, SessionName, SwayInstance, TerminalName};
use watch_unit::{NoWatcher, SystemdWatcher, Watcher};
use window::Mark;
use wm::{Wm, WorkspaceInfo};

const FORCE_HELP: &str = "Disconnect any other client attached to these shells";

/// Exit status of `attach` when several sessions fit and none was named.
const EXIT_CHOOSE: u8 = 2;

/// Exit status of `status` when the focused workspace is not bound.
const EXIT_UNBOUND: u8 = 3;

/// How long `detach` waits for closed windows' shells to lose their clients.
const DISCONNECT_WAIT: Duration = Duration::from_secs(5);
const DISCONNECT_POLL: Duration = Duration::from_millis(100);

#[derive(Parser)]
#[command(
    name = "i3mux",
    version,
    about = "Persistent terminal sessions for Sway workspaces",
    after_help = "Exit status: 0 success, 1 error, 2 usage error or (attach) several sessions to choose from, \
                  3 (status) the focused workspace is not bound."
)]
struct Cli {
    /// Print what i3mux does on stderr
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Bind the focused workspace to a session and open its first terminal
    Activate {
        /// Run the shells on this ssh host, e.g. 'deepthought' or 'user@host'
        #[arg(short, long)]
        remote: Option<String>,
        /// Session name [default: wsN for workspace N]
        #[arg(short, long)]
        session: Option<String>,
    },
    /// Open a terminal: a persistent shell in a bound workspace, else a plain terminal
    Terminal {
        /// Run CMD instead of the login shell; always starts a new shell
        #[arg(short = 'e', long = "exec", value_name = "CMD")]
        exec: Option<String>,
        /// Start a new shell even if a shell without a window exists
        #[arg(long)]
        new: bool,
        #[arg(long, help = FORCE_HELP)]
        force: bool,
    },
    /// Save the focused workspace's layout on the session's host and close its windows
    Detach,
    /// Restore a saved session into the focused workspace
    Attach {
        /// The host that runs the shells [default: this machine]
        #[arg(short, long)]
        remote: Option<String>,
        /// Session to restore [default: the only saved session not bound here]
        #[arg(short, long)]
        session: Option<String>,
        #[arg(long, help = FORCE_HELP)]
        force: bool,
    },
    /// List the sessions on a host
    Sessions {
        /// The host to list [default: this machine]; with --all, one more host to list
        #[arg(short, long)]
        remote: Option<String>,
        /// List this machine and every host that a workspace is bound to
        #[arg(short, long)]
        all: bool,
    },
    /// Show the session of the focused workspace and its ssh connection, without contacting its host
    Status,
    /// End a session: close its windows, kill its shells, delete its saved layout
    Kill {
        /// The host that runs the shells [default: this machine]
        #[arg(short, long, requires = "session")]
        remote: Option<String>,
        /// Session to end [default: the session of the focused workspace]
        #[arg(short, long)]
        session: Option<String>,
    },
    /// Keep backups of this Sway's workspace layouts; i3mux runs it as a user unit
    #[command(hide = true)]
    Watch,
}

/// Debug output on stderr, enabled by `-v`.
#[derive(Debug, Clone, Copy)]
struct Log {
    verbose: bool,
}

impl Log {
    fn debug(self, message: impl FnOnce() -> String) {
        if self.verbose {
            eprintln!("i3mux: {}", message());
        }
    }
}

/// Gives the commands of a host: a real runner in production, a fake in tests.
type HostFactory = dyn Fn(&Host) -> Box<dyn HostExec> + Sync;

/// Everything a core function touches outside this process.
struct Ports<'a> {
    store: &'a StateStore,
    sway: &'a (dyn Sway + Sync),
    launcher: &'a (dyn Launcher + Sync),
    hosts: &'a HostFactory,
    /// The ssh masters of remote hosts (plan-ssh-recovery decision 1).
    masters: &'a (dyn Masters + Sync),
    log: Log,
    /// How long `detach` waits for shells to lose their clients.
    disconnect_wait: Duration,
    /// This machine's backups of bound workspaces (plan-layout-backup
    /// decision 3).
    backups: &'a BackupStore,
    /// The Sway this command runs in: the owner of the bindings whose
    /// windows it holds (plan-layout-backup decision 2).
    current_sway: &'a SwayInstance,
    /// Starts the layout watcher of `current_sway` (rule 17).
    watcher: &'a (dyn Watcher + Sync),
}

/// A command refused to act. The messages are the plan's user-facing text.
#[derive(Debug, thiserror::Error)]
enum CommandError {
    #[error("Workspace {0} is not bound to a session; run 'i3mux activate' first")]
    Unbound(String),
    #[error("Workspace '{workspace}' cannot be a session name ({reason}); pass --session NAME")]
    NoDefaultSession {
        workspace: String,
        reason: &'static str,
    },
    #[error(
        "Workspace {workspace} has no windows of session '{session}'; nothing to detach \
         (to end the session, run 'i3mux kill'{})",
        resume_hint(*.resumable)
    )]
    NothingToDetach {
        workspace: String,
        session: SessionName,
        /// Its binding is resumable (plan-layout-backup decision 4).
        resumable: bool,
    },
    /// A workspace bound to another session that `terminal` can resume.
    #[error("{0}, or resume it with 'i3mux terminal'")]
    ResumableWorkspaceTaken(BindError),
    /// A resume dropped a binding that no longer held its session, and the
    /// host has no saved layout to restore instead (plan-layout-backup
    /// rules 13 and 13a).
    #[error("{}", lost_without_layout(.session, .host, .workspace, *.lost))]
    LostWithoutLayout {
        session: SessionName,
        host: Host,
        workspace: String,
        lost: Lost,
    },
    /// A resume dropped a binding that no longer held its session, and could
    /// not restore the host's saved layout instead.
    #[error(
        "{} Restoring the layout last saved on {} failed: {cause}. \
         Workspace {workspace} is no longer bound. To take it, run '{}'.",
        .lost.sentence(.session, .host),
        place(.host),
        attach_command(.host, .session)
    )]
    LostNotRestored {
        session: SessionName,
        host: Host,
        workspace: String,
        lost: Lost,
        /// Why the restore failed, with its causes.
        cause: String,
    },
    #[error("Workspace {0} already has i3mux terminals; detach or close them first")]
    WorkspaceHasTerminals(String),
    #[error(
        "Session '{0}' is already attached from another terminal or host; use --force to take over"
    )]
    AlreadyAttached(TerminalName),
    #[error("No saved session '{session}' on {host}; run 'i3mux sessions' to list sessions")]
    NoSavedSession { session: SessionName, host: Host },
    #[error("No saved sessions on {0} that are not already bound here")]
    NoSavedSessions(Host),
    #[error("No session '{session}' on {host}")]
    NoSuchSession { session: SessionName, host: Host },
    #[error(
        "Session '{session}' is detached, but the windows of session {session} did not \
         disconnect from their shells ({terminals}) within {waited_ms} ms; \
         if 'i3mux attach' refuses them, add --force"
    )]
    StillAttached {
        session: SessionName,
        terminals: String,
        waited_ms: u128,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("i3mux: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Wire the real ports, run the command, then stop the masters it left unused.
fn run(cli: Cli) -> Result<ExitCode> {
    let wm = Wm::connect()?;
    let current_sway = wm.instance()?;
    let store = StateStore::from_env()?;
    let backups = BackupStore::from_env()?;
    let runtime_dir = runtime_dir()?;
    let private_dir = private_runtime_dir(&runtime_dir)?;
    let launcher = TerminalLauncher::install(terminal_program(), &private_dir)?;
    let inner: Box<dyn Masters + Send + Sync> = if master::systemd_user_manager_runs(&runtime_dir) {
        let auth_sock = std::env::var("SSH_AUTH_SOCK")
            .ok()
            .filter(|s| !s.is_empty());
        Box::new(SystemdMasters::new(private_dir.clone(), auth_sock))
    } else {
        Box::new(DirectSsh)
    };
    let masters = Arc::new(TrackedMasters::new(inner));
    let log = Log {
        verbose: cli.verbose,
    };
    let watcher: Box<dyn Watcher + Sync> = if master::systemd_user_manager_runs(&runtime_dir) {
        let exe = std::env::current_exe().context("cannot find the i3mux program")?;
        let argv = watch_unit::unit_argv(&exe, wm.socket(), store.path(), backups.dir());
        Box::new(SystemdWatcher::new(argv))
    } else {
        log.debug(|| "no systemd user manager: no layout watcher, so no backups".to_owned());
        Box::new(NoWatcher)
    };
    let config = RunnerConfig {
        local_home: dirs::home_dir().context("cannot find the home directory; set HOME")?,
        local_runtime_dir: Some(runtime_dir.clone()),
        ssh_control_dir: private_dir,
        masters: Arc::clone(&masters) as Arc<dyn Masters + Send + Sync>,
    };
    let hosts = move |host: &Host| -> Box<dyn HostExec> {
        Box::new(HostRunner::new(host.clone(), config.clone()))
    };
    let ports = Ports {
        store: &store,
        sway: &wm,
        launcher: &launcher,
        hosts: &hosts,
        masters: masters.as_ref(),
        log,
        disconnect_wait: DISCONNECT_WAIT,
        backups: &backups,
        current_sway: &current_sway,
        watcher: watcher.as_ref(),
    };
    with_unused_masters_stopped(&store, &masters, &current_sway, || {
        dispatch(&ports, &wm, &launcher, cli.command)
    })
}

fn dispatch(
    ports: &Ports,
    wm: &Wm,
    launcher: &TerminalLauncher,
    command: CliCommand,
) -> Result<ExitCode> {
    match command {
        CliCommand::Activate { remote, session } => cmd_activate(ports, wm, remote, session),
        CliCommand::Terminal { exec, new, force } => {
            let request = TerminalRequest {
                command: exec,
                new,
                force,
            };
            cmd_terminal(ports, wm, launcher, &request)
        }
        CliCommand::Detach => cmd_detach(ports, wm),
        CliCommand::Attach {
            remote,
            session,
            force,
        } => cmd_attach(ports, wm, remote, session, force),
        CliCommand::Sessions { remote, all } => cmd_sessions(ports, remote, all),
        CliCommand::Status => cmd_status(ports, wm),
        CliCommand::Kill { remote, session } => cmd_kill(ports, wm, remote, session),
        CliCommand::Watch => cmd_watch(ports, wm),
    }
}

/// Run `command`, then stop the ssh master of each host it touched that no
/// workspace is bound to (plan-ssh-recovery rule 12), also when it failed.
/// So a failed `activate` or `attach`, or `sessions --remote`, leaves no
/// master behind. Hosts the command did not touch keep their masters.
///
/// Only bindings that `current_sway` owns keep a master: a binding left by
/// an earlier Sway has no windows (plan-layout-backup rule 18). A binding
/// with no owner keeps it too: it comes from before owners, and its windows
/// may be in this Sway, cut off if their master stopped.
///
/// The stop runs after the command's last remote command, and under the
/// state lock, so no concurrent command binds the host between the check and
/// the stop. A command that touched no host takes no lock. The command's
/// error wins over a failed stop, which is then printed as a warning.
fn with_unused_masters_stopped<T>(
    store: &StateStore,
    masters: &TrackedMasters,
    current_sway: &SwayInstance,
    command: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let result = command();
    match (result, stop_unused_masters(store, masters, current_sway)) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(stop_err)) => Err(stop_err),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(stop_err)) => {
            eprintln!("i3mux: warning: {stop_err:#}");
            Err(err)
        }
    }
}

/// Stop the masters of the touched hosts that no binding of this Sway uses.
fn stop_unused_masters(
    store: &StateStore,
    masters: &TrackedMasters,
    current_sway: &SwayInstance,
) -> Result<()> {
    let touched = masters.touched_hosts();
    if touched.is_empty() {
        return Ok(());
    }
    let lock = store.lock()?;
    for remote in touched {
        let host = Host::Ssh(remote.clone());
        let in_use = lock
            .state()
            .bindings()
            .any(|(_, b)| b.host() == &host && b.owner().is_none_or(|owner| owner == current_sway));
        if !in_use {
            masters.stop(&remote)?;
        }
    }
    Ok(())
}

/// `$XDG_RUNTIME_DIR`.
fn runtime_dir() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .context(
            "XDG_RUNTIME_DIR is not set; i3mux keeps its terminal wrapper and ssh sockets there",
        )?;
    Ok(PathBuf::from(runtime))
}

/// `<runtime_dir>/i3mux`, created with mode 0700. Holds the terminal
/// wrapper and the ssh control sockets (plan decision 8: nothing in /tmp).
fn private_runtime_dir(runtime_dir: &Path) -> Result<PathBuf> {
    let dir = runtime_dir.join("i3mux");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict {}", dir.display()))?;
    Ok(dir)
}

/// `$TERMINAL`, or foot.
fn terminal_program() -> String {
    std::env::var("TERMINAL")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "foot".to_owned())
}

fn parse_session(session: Option<String>) -> Result<Option<SessionName>> {
    Ok(session.map(SessionName::new).transpose()?)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------- commands

fn cmd_activate(
    ports: &Ports,
    wm: &Wm,
    remote: Option<String>,
    session: Option<String>,
) -> Result<ExitCode> {
    let host = Host::from_remote_arg(remote)?;
    let session = parse_session(session)?;
    let workspace = wm.focused_workspace()?;
    let (session, choice) = activate(ports, &host, &workspace, session)?;
    println!(
        "✓ Workspace {} bound to session '{session}' on {host}",
        workspace.name()
    );
    println!("  {}", describe_choice(&choice));
    Ok(ExitCode::SUCCESS)
}

fn cmd_terminal(
    ports: &Ports,
    wm: &Wm,
    launcher: &TerminalLauncher,
    request: &TerminalRequest,
) -> Result<ExitCode> {
    let workspace = wm.focused_workspace()?;
    if ports.store.load()?.get(workspace.name()).is_none() {
        launcher.spawn_plain(request.command.as_deref())?;
        return Ok(ExitCode::SUCCESS);
    }
    match terminal(ports, workspace.name(), request)? {
        TerminalOutcome::Opened(choice) => ports.log.debug(|| describe_choice(&choice)),
        TerminalOutcome::Restored {
            attached,
            then_opened,
        } => {
            let then = then_opened.map(|_| opened_label(request));
            report_attached(&attached, workspace.name(), then.as_deref()).print();
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// What a command prints: notices on stderr, results on stdout.
#[derive(Debug, Default, PartialEq, Eq)]
struct Report {
    notices: Vec<String>,
    results: Vec<String>,
}

impl Report {
    fn print(&self) {
        for notice in &self.notices {
            eprintln!("{notice}");
        }
        for result in &self.results {
            println!("{result}");
        }
    }
}

fn cmd_detach(ports: &Ports, wm: &Wm) -> Result<ExitCode> {
    let workspace = wm.focused_workspace()?;
    let detached = detach(ports, workspace.name(), now_unix())?;
    report_detached(&detached).print();
    Ok(ExitCode::SUCCESS)
}

/// What `detach` prints. A session that another machine took since, or
/// that ended, is closed here without a save (plan-layout-backup rule 5);
/// a notice says so, and the exit status stays 0.
fn report_detached(detached: &Detached) -> Report {
    let terminals = count(detached.terminals, "terminal", "terminals");
    let (session, host) = (&detached.session, place(&detached.host));
    let closed = format!("Closed session '{session}' here. Not saved");
    match detached.token {
        TokenMatch::Same => Report {
            results: vec![format!(
                "✓ Detached session '{session}': layout of {terminals} saved on {host}"
            )],
            ..Report::default()
        },
        TokenMatch::TakenElsewhere => Report {
            notices: vec![format!(
                "{closed}: another machine took it, so {host} keeps that machine's layout."
            )],
            ..Report::default()
        },
        TokenMatch::Ended => Report {
            notices: vec![format!("{closed}: {session} ended on {host}.")],
            ..Report::default()
        },
    }
}

fn cmd_attach(
    ports: &Ports,
    wm: &Wm,
    remote: Option<String>,
    session: Option<String>,
    force: bool,
) -> Result<ExitCode> {
    let remote = remote.map(RemoteHost::new).transpose()?;
    let host = remote.clone().map_or(Host::Local, Host::Ssh);
    let session = parse_session(session)?;
    let workspace = wm.focused_workspace()?;
    match attach(ports, remote.as_ref(), workspace.name(), session, force)? {
        AttachOutcome::Choose(candidates) => {
            eprintln!("Several saved sessions on {host}:");
            for name in &candidates {
                eprintln!("  {name}");
            }
            eprintln!("Choose one with --session NAME.");
            Ok(ExitCode::from(EXIT_CHOOSE))
        }
        AttachOutcome::Attached(attached) => {
            report_attached(&attached, workspace.name(), None).print();
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// What `attach`, or a `terminal` that brought windows back, prints. A lost
/// binding is a notice (plan-layout-backup rules 13 and 13a). `then` names
/// the terminal that `terminal -e` or `--new` opened after the restore.
fn report_attached(attached: &Attached, workspace: &str, then: Option<&str>) -> Report {
    let Attached {
        session,
        host,
        total,
        recreated,
        source,
    } = attached;
    let terminals = count(*total, "terminal", "terminals");
    let place = place(host);
    let headline = match source {
        LayoutSource::Host | LayoutSource::HostAfterLoss(_) => {
            format!("✓ Attached session '{session}' from {place} to workspace {workspace}")
        }
        LayoutSource::HostWithoutBackup => format!(
            "✓ Attached session '{session}' from the layout saved on {place} to workspace {workspace}"
        ),
        LayoutSource::Backup => {
            format!("✓ Resumed session '{session}' from backup on {place} in workspace {workspace}")
        }
    };
    let then = then.map(|label| format!(", then opened {label}"));
    let mut results = vec![format!(
        "{headline} ({terminals}){}",
        then.unwrap_or_default()
    )];
    if *recreated > 0 {
        results.push(format!(
            "  recreated {recreated} of {total} shells: the host had lost them, for example after a reboot"
        ));
    }
    let notices = match source {
        LayoutSource::HostAfterLoss(lost) => vec![format!(
            "{} Restoring the layout last saved on {place}.",
            lost.sentence(session, host)
        )],
        LayoutSource::Host | LayoutSource::HostWithoutBackup | LayoutSource::Backup => vec![],
    };
    Report { notices, results }
}

/// How the output names the terminal that `terminal -e` or `--new` opened.
fn opened_label(request: &TerminalRequest) -> String {
    request.command.as_ref().map_or_else(
        || "a new terminal".to_owned(),
        |command| format!("'{command}'"),
    )
}

/// How messages about the lost, resumed or detached sessions of a host name
/// it: the host, or "this machine".
fn place(host: &Host) -> &str {
    match host {
        Host::Local => "this machine",
        Host::Ssh(remote) => remote.as_str(),
    }
}

/// `--remote HOST `, or nothing for this machine, for commands in messages.
fn remote_flag(host: &Host) -> String {
    match host {
        Host::Local => String::new(),
        Host::Ssh(remote) => format!("--remote {remote} "),
    }
}

/// The command that takes `session` on `host` from its saved layout.
fn attach_command(host: &Host, session: &SessionName) -> String {
    format!(
        "i3mux attach {}--session {session} --force",
        remote_flag(host)
    )
}

/// The command that binds the focused workspace to a session on `host`:
/// `session`, or the workspace's default.
fn activate_command(host: &Host, session: Option<&SessionName>) -> String {
    let session = session
        .map(|s| format!("--session {s}"))
        .unwrap_or_default();
    format!("i3mux activate {}{session}", remote_flag(host))
        .trim_end()
        .to_owned()
}

/// [`CommandError::LostWithoutLayout`]: what happened, and the remedy.
fn lost_without_layout(session: &SessionName, host: &Host, workspace: &str, lost: Lost) -> String {
    let place = place(host);
    match lost {
        Lost::Ended => format!(
            "{session} ended on {place} (the host restarted, or the session was killed) and has \
             no saved layout. Workspace {workspace} is unbound. Start a new session with '{}'.",
            activate_command(host, None)
        ),
        Lost::TakenElsewhere => format!(
            "{session} was taken by another machine after your windows closed, and {place} has \
             no saved layout of it. Workspace {workspace} is unbound. To take it, run '{}'.",
            activate_command(host, Some(session))
        ),
    }
}

/// The end of [`CommandError::NothingToDetach`] for a resumable binding.
fn resume_hint(resumable: bool) -> &'static str {
    if resumable {
        "; or run 'i3mux terminal' to bring its windows back"
    } else {
        ""
    }
}

fn cmd_sessions(ports: &Ports, remote: Option<String>, all: bool) -> Result<ExitCode> {
    let (rows, listed) = if all {
        let extra = remote.map(RemoteHost::new).transpose()?.map(Host::Ssh);
        let listing = sessions_all(ports, extra)?;
        for (host, err) in &listing.failed {
            eprintln!("i3mux: warning: cannot list the sessions on {host}: {err:#}");
        }
        (listing.rows, listing.listed)
    } else {
        let host = Host::from_remote_arg(remote)?;
        (sessions(ports, &host)?, vec![host])
    };
    if rows.is_empty() {
        let hosts: Vec<String> = listed.iter().map(ToString::to_string).collect();
        eprintln!("No sessions on {}", hosts.join(", "));
    }
    for line in format_rows(&rows) {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_status(ports: &Ports, wm: &Wm) -> Result<ExitCode> {
    let workspace = wm.focused_workspace()?;
    let Some(report) = status(ports, workspace.name())? else {
        println!("Workspace {}: no session", workspace.name());
        return Ok(ExitCode::from(EXIT_UNBOUND));
    };
    for line in format_status(&report) {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_kill(
    ports: &Ports,
    wm: &Wm,
    remote: Option<String>,
    session: Option<String>,
) -> Result<ExitCode> {
    let (host, session) = if let Some(session) = parse_session(session)? {
        (Host::from_remote_arg(remote)?, session)
    } else {
        let workspace = wm.focused_workspace()?;
        let binding = ports
            .store
            .load()?
            .get(workspace.name())
            .cloned()
            .ok_or_else(|| CommandError::Unbound(workspace.name().to_owned()))?;
        (binding.host().clone(), binding.session().clone())
    };
    let killed = kill(ports, &host, &session)?;
    println!(
        "✓ Killed session '{session}' on {host}: {}, {}{}",
        count(killed.shells, "shell", "shells"),
        count(killed.windows, "window", "windows"),
        if killed.layout_deleted {
            ", saved layout deleted"
        } else {
            ""
        }
    );
    Ok(ExitCode::SUCCESS)
}

/// `i3mux watch`, which the unit of [`start_watcher`] runs: keep the
/// backups of this Sway's bindings until Sway exits (plan-layout-backup
/// rules 6-8, 10 and 17). Exit 0 on Sway's shutdown, or when Sway closes its
/// socket; any other end is an error, and systemd restarts the watcher.
fn cmd_watch(ports: &Ports, wm: &Wm) -> Result<ExitCode> {
    let mut swaymsg = wm.subscribe(watch::SWAY_EVENTS)?;
    let output = swaymsg
        .stdout
        .take()
        .context("swaymsg started without its output")?;
    let mut events = watch::SwaymsgEvents::read(output);
    let stop = watch::watch(&mut events, || {
        watch::back_up(ports.store, ports.sway, ports.backups, ports.current_sway)
    });
    if !matches!(stop, Ok(watch::Stop::Closed)) {
        if let Err(err) = swaymsg.kill() {
            ports.log.debug(|| format!("cannot stop swaymsg: {err}"));
        }
    }
    let status = swaymsg.wait().context("cannot wait for swaymsg")?;
    match stop? {
        watch::Stop::Shutdown => {}
        watch::Stop::Closed => watch::check_closed(events.subscribed(), status)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn describe_choice(choice: &Choice) -> String {
    match choice {
        Choice::Reattach(name) => format!("reattached shell {name}"),
        Choice::Create(name) => format!("started shell {name}"),
    }
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ---------------------------------------------------------------- core

/// A terminal to open in a bound workspace.
#[derive(Debug, Default)]
struct TerminalRequest {
    /// Run this instead of the login shell (always a new shell).
    command: Option<String>,
    /// Start a new shell even if one has no window.
    new: bool,
    /// Take over shells attached elsewhere.
    force: bool,
}

/// `wsN` for a numbered workspace, else the workspace name itself.
/// A name with characters outside `[A-Za-z0-9_-]` (such as `1:web`) has no
/// default, and the caller must pass `--session`.
fn default_session(workspace: &WorkspaceInfo) -> Result<SessionName, CommandError> {
    if let Some(number) = workspace.number() {
        return Ok(SessionName::for_workspace(number));
    }
    SessionName::new(workspace.name()).map_err(|err| CommandError::NoDefaultSession {
        workspace: workspace.name().to_owned(),
        reason: match err {
            InvalidName::Session { reason, .. } => reason,
            InvalidName::Host { .. } | InvalidName::Terminal(_) => "invalid name",
        },
    })
}

/// Bind `workspace` to `session` on `host`, then open its first terminal.
/// The host is checked (reachable, shpool installed) before anything is
/// bound, and the session is taken on it (plan-layout-backup rule 1).
fn activate(
    ports: &Ports,
    host: &Host,
    workspace: &WorkspaceInfo,
    session: Option<SessionName>,
) -> Result<(SessionName, Choice)> {
    let session = match session {
        Some(session) => session,
        None => default_session(workspace)?,
    };
    let exec = (ports.hosts)(host);
    mux::list(exec.as_ref())?;
    ports.store.with_lock(|state| {
        take_and_bind(state, ports, exec.as_ref(), workspace.name(), &session)
    })?;
    start_watcher(ports);
    let choice = open_terminal(ports, workspace.name(), &TerminalRequest::default())?;
    Ok((session, choice))
}

/// Refuse a bind that guarantee 1 of `state` forbids, else take `session`
/// on its host and bind `workspace` to it with the new token, owned by this
/// Sway. The caller holds the state lock and commits (plan-layout-backup
/// rule 2). A resume never calls this (rule 3).
///
/// If the take fails, `state` is unchanged. If the commit fails after the
/// take, no binding holds the new token, and the next take replaces it.
fn take_and_bind(
    state: &mut LocalState,
    ports: &Ports,
    exec: &dyn HostExec,
    workspace: &str,
    session: &SessionName,
) -> Result<()> {
    if let Err(refusal) = state.check_bind(workspace, exec.host(), session) {
        return Err(bind_refusal(ports, state, workspace, refusal));
    }
    let token = session::take(exec, session)?;
    let owner = ports.current_sway.clone();
    let binding = WorkspaceState::new(exec.host().clone(), session.clone(), token, owner);
    state.bind(workspace, binding)?;
    Ok(())
}

/// `refusal`, with the hint to resume the workspace if its binding is
/// resumable. A failed Sway query only drops the hint.
fn bind_refusal(
    ports: &Ports,
    state: &LocalState,
    workspace: &str,
    refusal: BindError,
) -> anyhow::Error {
    let resumable = matches!(refusal, BindError::WorkspaceTaken { .. })
        && state.get(workspace).is_some_and(|binding| {
            let tree = ports.sway.get_tree();
            tree.is_ok_and(|tree| is_resumable(binding, ports.current_sway, &tree))
        });
    if resumable {
        CommandError::ResumableWorkspaceTaken(refusal).into()
    } else {
        refusal.into()
    }
}

/// Start the layout watcher of this Sway, once a binding that it owns is
/// committed (plan-layout-backup rule 17). A failure only warns: the
/// windows matter more than their backup.
fn start_watcher(ports: &Ports) {
    if let Err(err) = ports.watcher.ensure_running() {
        eprintln!("i3mux: warning: {err:#}; the layouts of this Sway are not backed up");
    }
}

/// Open one window of the session bound to `workspace` (plan rule 4).
///
/// Under the state lock: list the host's shells, read the marked windows,
/// choose (lowest orphan, or a new number), spawn, wait for the window,
/// mark it. The mark is visible before the lock is released, so a
/// concurrent call never picks the same shell.
///
/// The window is in this Sway, so this Sway then owns the binding
/// (plan-layout-backup decision 2). That also adopts a binding from before
/// owners whose windows are here.
fn open_terminal(ports: &Ports, workspace: &str, request: &TerminalRequest) -> Result<Choice> {
    let mut lock = ports.store.lock()?;
    let binding = lock
        .state()
        .get(workspace)
        .cloned()
        .ok_or_else(|| CommandError::Unbound(workspace.to_owned()))?;
    let host = (ports.hosts)(binding.host());
    let shells: Vec<Shell> = mux::list(host.as_ref())?
        .into_iter()
        .filter(|s| s.name().session() == binding.session())
        .collect();
    let mut in_use =
        window::session_terminals(&ports.sway.get_tree()?, binding.host(), binding.session());
    if !request.force {
        in_use.extend(
            shells
                .iter()
                .filter(|s| s.is_attached())
                .map(|s| s.name().clone()),
        );
    }
    let alive: Vec<TerminalName> = shells.iter().map(|s| s.name().clone()).collect();
    let force_new = request.new || request.command.is_some();
    let choice = mux::choose_terminal(binding.session(), &alive, &in_use, force_new);
    open_window(
        ports,
        host.as_ref(),
        choice.name(),
        request.force,
        request.command.as_deref(),
    )?;
    if !binding.is_owned_by(ports.current_sway) {
        // After an interrupted resume this claims a partial layout, which
        // the watcher then backs up over the full one: accepted.
        let owned = binding.with_owner(ports.current_sway.clone());
        lock.state_mut().bind(workspace, owned)?;
        lock.commit()?;
        start_watcher(ports);
    }
    Ok(choice)
}

/// What `terminal` did in a bound workspace.
#[derive(Debug)]
enum TerminalOutcome {
    /// It opened one window (plan rule 4).
    Opened(Choice),
    /// It brought the windows of a resumable binding back
    /// (plan-layout-backup rule 11), then, for `-e` or `--new`, opened the
    /// requested terminal too.
    Restored {
        attached: Attached,
        then_opened: Option<Choice>,
    },
}

/// Open a terminal in the bound `workspace`, or, if its binding is
/// resumable, resume it instead (plan-layout-backup rule 11). After a
/// resume, `-e` or `--new` still opens the requested terminal; a plain
/// request opens nothing more.
///
/// With no backup (rule 15), it restores the layout saved on the host, or,
/// if there is none, opens the requested terminal as for a binding of this
/// Sway. Both reattach with force: the host token is still the binding's,
/// so no other machine took the session since.
fn terminal(ports: &Ports, workspace: &str, request: &TerminalRequest) -> Result<TerminalOutcome> {
    let attached = match resume(ports, workspace, request.force, |_| true)? {
        Resume::NotResumable => {
            return open_terminal(ports, workspace, request).map(TerminalOutcome::Opened);
        }
        Resume::Restored(attached) => attached,
        Resume::NoBackup(binding) => {
            let exec = (ports.hosts)(binding.host());
            let Some(saved) = session::load(exec.as_ref(), binding.session())? else {
                let forced = TerminalRequest {
                    command: request.command.clone(),
                    new: request.new,
                    force: true,
                };
                return open_terminal(ports, workspace, &forced).map(TerminalOutcome::Opened);
            };
            let attached = attach_saved(ports, exec.as_ref(), workspace, &saved, true)?;
            Attached {
                source: LayoutSource::HostWithoutBackup,
                ..attached
            }
        }
    };
    let then_opened = open_after_restore(ports, workspace, request, &attached)?;
    Ok(TerminalOutcome::Restored {
        attached,
        then_opened,
    })
}

/// After a restore, open the terminal that `-e` or `--new` asked for.
fn open_after_restore(
    ports: &Ports,
    workspace: &str,
    request: &TerminalRequest,
    restored: &Attached,
) -> Result<Option<Choice>> {
    if request.command.is_none() && !request.new {
        return Ok(None);
    }
    let choice = open_terminal(ports, workspace, request).with_context(|| {
        format!(
            "restored session '{}' in workspace {workspace}, but did not open {}",
            restored.session,
            opened_label(request)
        )
    })?;
    Ok(Some(choice))
}

/// What [`resume`] found or did.
#[derive(Debug)]
enum Resume {
    /// The workspace has no resumable binding that the caller wants.
    NotResumable,
    /// The windows are back (plan-layout-backup rules 12 and 13).
    Restored(Attached),
    /// The host token is the binding's, but this machine has no backup of
    /// it (rule 15). The caller restores the windows another way.
    NoBackup(WorkspaceState),
}

/// Whether `binding` is resumable: this Sway does not own it, and has no
/// window of its session (plan-layout-backup decision 4). Its windows went
/// away with an earlier Sway. A binding from before owners whose windows
/// are here is not resumable.
fn is_resumable(binding: &WorkspaceState, current_sway: &SwayInstance, tree: &Value) -> bool {
    !binding.is_owned_by(current_sway)
        && window::session_terminals(tree, binding.host(), binding.session()).is_empty()
}

/// Resume the binding of `workspace` if it is resumable and `wanted`
/// (plan-layout-backup rules 11-14).
///
/// Under the state lock, it refuses a workspace with other i3mux windows
/// (plan rule 6), then reads the host token. It never writes the token
/// (rule 3).
/// - The host fails, or its token is unreadable: the error, and the binding
///   and the backup do not change (rule 14).
/// - Another token, or no token file: drop the binding and its backup, then
///   attach from the host layout, which takes the session; `force` applies
///   there (rules 13 and 13a).
/// - The binding's token and a backup: [`restore_backup`] (rule 12).
/// - The binding's token, no backup: [`Resume::NoBackup`] (rule 15).
fn resume(
    ports: &Ports,
    workspace: &str,
    force: bool,
    wanted: impl FnOnce(&WorkspaceState) -> bool,
) -> Result<Resume> {
    let mut lock = ports.store.lock()?;
    let Some(binding) = lock.state().get(workspace).cloned().filter(wanted) else {
        return Ok(Resume::NotResumable);
    };
    let tree = ports.sway.get_tree()?;
    if !is_resumable(&binding, ports.current_sway, &tree) {
        return Ok(Resume::NotResumable);
    }
    refuse_i3mux_windows(&tree, workspace)?;
    let exec = (ports.hosts)(binding.host());
    let host_token = session::read_token(exec.as_ref(), binding.session())?;
    let lost = match session::compare_tokens(binding.token(), host_token.as_ref()) {
        TokenMatch::Same => None,
        TokenMatch::TakenElsewhere => Some(Lost::TakenElsewhere),
        TokenMatch::Ended => Some(Lost::Ended),
    };
    if let Some(lost) = lost {
        lock.state_mut().unbind(workspace);
        lock.commit()?;
        ports.backups.remove(binding.host(), binding.session())?;
        drop(lock);
        return attach_lost_binding(ports, &binding, workspace, force, lost).map(Resume::Restored);
    }
    match ports.backups.read(&binding)? {
        Some(layout) => restore_backup(
            ports,
            &mut lock,
            exec.as_ref(),
            workspace,
            &binding,
            &layout,
        )
        .map(Resume::Restored),
        None => Ok(Resume::NoBackup(binding)),
    }
}

/// Restore `layout`, the backup of `binding`, into `workspace`, then make
/// this Sway the binding's owner (plan-layout-backup rule 12).
///
/// The owner is set only after the last window is marked. An interrupted
/// restore leaves the binding resumable and the backup unchanged, because
/// only bindings of this Sway get new backups (rule 6).
///
/// It reattaches with force: no take happened since the binding's own, so
/// an attached client is a leftover of the binding's earlier windows. A
/// host keeps those until it notices the dead link.
fn restore_backup(
    ports: &Ports,
    lock: &mut StateLock,
    exec: &dyn HostExec,
    workspace: &str,
    binding: &WorkspaceState,
    layout: &Layout,
) -> Result<Attached> {
    let recreated = check_layout_shells(exec, layout, true)?;
    open_layout(ports, exec, layout, true)?;
    let owned = binding.clone().with_owner(ports.current_sway.clone());
    lock.state_mut().bind(workspace, owned)?;
    lock.commit()?;
    start_watcher(ports);
    Ok(Attached {
        session: binding.session().clone(),
        host: binding.host().clone(),
        total: layout.terminals().len(),
        recreated,
        source: LayoutSource::Backup,
    })
}

/// After the binding of `workspace` was `lost` and dropped: attach the
/// layout saved on the host (plan-layout-backup rules 13 and 13a). Every
/// error says that the workspace is unbound, and how to go on:
/// [`CommandError::LostWithoutLayout`] or [`CommandError::LostNotRestored`].
fn attach_lost_binding(
    ports: &Ports,
    binding: &WorkspaceState,
    workspace: &str,
    force: bool,
    lost: Lost,
) -> Result<Attached> {
    let (session, host) = (binding.session(), binding.host());
    let exec = (ports.hosts)(host);
    let restored = session::load(exec.as_ref(), session)
        .map_err(anyhow::Error::from)
        .and_then(|saved| {
            saved
                .map(|saved| attach_saved(ports, exec.as_ref(), workspace, &saved, force))
                .transpose()
        });
    match restored {
        Ok(Some(attached)) => Ok(Attached {
            source: LayoutSource::HostAfterLoss(lost),
            ..attached
        }),
        Ok(None) => Err(CommandError::LostWithoutLayout {
            session: session.clone(),
            host: host.clone(),
            workspace: workspace.to_owned(),
            lost,
        }
        .into()),
        Err(cause) => Err(CommandError::LostNotRestored {
            session: session.clone(),
            host: host.clone(),
            workspace: workspace.to_owned(),
            lost,
            cause: format!("{cause:#}"),
        }
        .into()),
    }
}

/// Launch the window of `terminal` and mark it.
fn open_window(
    ports: &Ports,
    host: &dyn HostExec,
    terminal: &TerminalName,
    force: bool,
    command: Option<&str>,
) -> Result<()> {
    let mark = Mark::new(host.host().clone(), terminal.clone());
    let window = window_command(ports, host, terminal, force, command)?;
    ports
        .log
        .debug(|| format!("opening {mark}: {}", shell_words::join(window.argv())));
    let title = format!("i3mux:{}:{}", mark.host(), mark.terminal());
    ports.launcher.spawn(&mark.app_id(), &title, &window)?;
    sway_ops::wait_for_window_and_mark(ports.sway, &mark)?;
    Ok(())
}

/// What the window of `terminal` runs. A remote window goes through its
/// host's ssh master, which this starts first (plan-ssh-recovery rule 1),
/// and gets the `--force` argv that it reattaches with after a dropped
/// connection (rule 8).
fn window_command(
    ports: &Ports,
    host: &dyn HostExec,
    terminal: &TerminalName,
    force: bool,
    command: Option<&str>,
) -> Result<WindowCommand> {
    let argv = mux::attach_argv(host, terminal, force, command);
    let Host::Ssh(remote) = host.host() else {
        return Ok(WindowCommand::Plain(argv));
    };
    Ok(match ports.masters.ensure_started(remote)? {
        Link::Direct => WindowCommand::Plain(argv),
        Link::Shared(unit) => WindowCommand::Remote(RemoteAttach {
            argv,
            reattach: mux::attach_argv(host, terminal, true, command),
            unit,
            host: remote.clone(),
            shell: terminal.clone(),
        }),
    })
}

#[derive(Debug)]
struct Detached {
    session: SessionName,
    host: Host,
    /// Terminals in the captured layout.
    terminals: usize,
    /// What the host's token was. Only [`TokenMatch::Same`] saved the
    /// layout (plan-layout-backup rules 4 and 5).
    token: TokenMatch,
}

/// Save the layout of `workspace` on the session's host, close its windows,
/// disconnect their shells, and unbind it. Its backup goes with the binding
/// (plan-layout-backup rule 16).
///
/// Plan rule 5: at least one i3mux window is required. Plan rule 7: the
/// layout is written before any window closes, so a failed write leaves
/// every window open and the workspace bound.
///
/// The write happens only while the host has the binding's token
/// (plan-layout-backup rule 4). If another take changed it, or the session
/// ended, the windows still close and the workspace is unbound, but the
/// host layout and the shells' clients are not touched: they belong to the
/// machine that took the session, or to nobody (rule 5).
///
/// Sway closes windows asynchronously, so their shpool clients can outlive
/// the kill; an `attach` right after would then see the shells as attached
/// elsewhere and refuse them. So `detach` disconnects the closed windows'
/// shells with `shpool detach` and waits, still under the lock, until
/// `shpool list` shows none of them attached. The binding is committed
/// before the wait: if the wait times out, the session is detached and the
/// error only warns that the next `attach` may need `--force`.
fn detach(ports: &Ports, workspace: &str, saved_at_unix: u64) -> Result<Detached> {
    let mut lock = ports.store.lock()?;
    let binding = lock
        .state()
        .get(workspace)
        .cloned()
        .ok_or_else(|| CommandError::Unbound(workspace.to_owned()))?;
    let (host, name) = (binding.host(), binding.session());
    let tree = ports.sway.get_tree()?;
    let layout = layout::capture_workspace(&tree, workspace, host, name).ok_or_else(|| {
        CommandError::NothingToDetach {
            workspace: workspace.to_owned(),
            session: name.clone(),
            resumable: is_resumable(&binding, ports.current_sway, &tree),
        }
    })?;
    let saved = SavedSession::new(name.clone(), host.clone(), layout, saved_at_unix)?;
    let exec = (ports.hosts)(host);
    let token = session::save_if_token(exec.as_ref(), &saved, binding.token())?;
    let closed = sway_ops::kill_all(ports.sway, host, name)?;
    match token {
        TokenMatch::Same => {
            mux::detach(exec.as_ref(), &closed)?;
            lock.state_mut().unbind(workspace);
            lock.commit()?;
            ports.backups.remove(host, name)?;
            wait_until_disconnected(exec.as_ref(), name, &closed, ports.disconnect_wait)?;
        }
        TokenMatch::TakenElsewhere | TokenMatch::Ended => {
            lock.state_mut().unbind(workspace);
            lock.commit()?;
            ports.backups.remove(host, name)?;
        }
    }
    Ok(Detached {
        session: name.clone(),
        host: host.clone(),
        terminals: saved.layout().terminals().len(),
        token,
    })
}

/// Poll `shpool list` until none of `names` has a client.
fn wait_until_disconnected(
    exec: &dyn HostExec,
    session: &SessionName,
    names: &[TerminalName],
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let attached: Vec<String> = mux::list(exec)?
            .into_iter()
            .filter(|s| s.is_attached() && names.contains(s.name()))
            .map(|s| s.name().to_string())
            .collect();
        if attached.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CommandError::StillAttached {
                session: session.clone(),
                terminals: attached.join(", "),
                waited_ms: timeout.as_millis(),
            }
            .into());
        }
        std::thread::sleep(DISCONNECT_POLL);
    }
}

#[derive(Debug)]
enum AttachOutcome {
    Attached(Attached),
    /// Several saved sessions fit; the user must name one.
    Choose(Vec<SessionName>),
}

/// Why a binding no longer held its session when a resume read the host
/// token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lost {
    /// Another machine took the session since (plan-layout-backup rule 13).
    TakenElsewhere,
    /// The session ended on its host (rule 13a).
    Ended,
}

impl Lost {
    /// The sentence that tells the user what happened.
    fn sentence(self, session: &SessionName, host: &Host) -> String {
        match self {
            Self::TakenElsewhere => {
                format!("{session} was taken by another machine after your windows closed.")
            }
            Self::Ended => format!(
                "{session} ended on {} (the host restarted, or the session was killed).",
                place(host)
            ),
        }
    }
}

/// Where the layout that `attach` or `terminal` restored came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutSource {
    /// The layout saved on the host.
    Host,
    /// The layout saved on the host, after the workspace's binding was lost
    /// (plan-layout-backup rules 13 and 13a).
    HostAfterLoss(Lost),
    /// The layout saved on the host, for a resumable binding that this
    /// machine has no backup of (rule 15).
    HostWithoutBackup,
    /// This machine's backup: a resume (rule 12).
    Backup,
}

#[derive(Debug)]
struct Attached {
    session: SessionName,
    host: Host,
    total: usize,
    /// Shells the host no longer had, started fresh (plan rule 11).
    recreated: usize,
    source: LayoutSource,
}

/// Restore a session of `remote` (this machine if `None`) into `workspace`.
///
/// A resumable binding of the workspace is resumed (plan-layout-backup
/// rule 11) when the request leaves it open: a bare `attach` resumes it
/// whatever its host, and `--remote` or `--session` must match it. Without
/// a backup it restores that session's host layout, with force, as
/// [`terminal`] does (rule 15). Otherwise it restores a saved session, as
/// [`attach_from_host`].
fn attach(
    ports: &Ports,
    remote: Option<&RemoteHost>,
    workspace: &str,
    requested: Option<SessionName>,
    force: bool,
) -> Result<AttachOutcome> {
    let host = remote.cloned().map_or(Host::Local, Host::Ssh);
    let bare = remote.is_none() && requested.is_none();
    let wanted = |binding: &WorkspaceState| {
        bare || (binding.host() == &host
            && requested.as_ref().is_none_or(|s| s == binding.session()))
    };
    match resume(ports, workspace, force, wanted)? {
        Resume::Restored(attached) => Ok(AttachOutcome::Attached(attached)),
        Resume::NoBackup(binding) => {
            let attached = attach_named(ports, binding.host(), workspace, binding.session(), true)?;
            Ok(AttachOutcome::Attached(Attached {
                source: LayoutSource::HostWithoutBackup,
                ..attached
            }))
        }
        Resume::NotResumable => attach_from_host(ports, &host, workspace, requested, force),
    }
}

/// Restore the saved session `requested` of `host` into `workspace`, or,
/// with no name, the only saved session that is not bound here.
fn attach_from_host(
    ports: &Ports,
    host: &Host,
    workspace: &str,
    requested: Option<SessionName>,
    force: bool,
) -> Result<AttachOutcome> {
    let name = match requested {
        Some(name) => name,
        None => match pick_saved_session(ports, (ports.hosts)(host).as_ref())? {
            Pick::One(name) => name,
            Pick::Several(candidates) => return Ok(AttachOutcome::Choose(candidates)),
        },
    };
    attach_named(ports, host, workspace, &name, force).map(AttachOutcome::Attached)
}

/// Restore the saved session `name` of `host` into `workspace`.
fn attach_named(
    ports: &Ports,
    host: &Host,
    workspace: &str,
    name: &SessionName,
    force: bool,
) -> Result<Attached> {
    let exec = (ports.hosts)(host);
    let saved =
        session::load(exec.as_ref(), name)?.ok_or_else(|| CommandError::NoSavedSession {
            session: name.clone(),
            host: host.clone(),
        })?;
    attach_saved(ports, exec.as_ref(), workspace, &saved, force)
}

/// Restore `saved` into `workspace` and bind the workspace to it.
///
/// Refuses a shell attached elsewhere unless `force` (plan rule 10), and a
/// workspace that already has i3mux windows (plan rule 6). After the last
/// refusal, under the state lock, it takes the session on the host
/// (plan-layout-backup rules 1 and 2). The workspace is bound before any
/// window opens, so an interrupted attach leaves a bound workspace that
/// `detach` or `kill` can clean up.
fn attach_saved(
    ports: &Ports,
    exec: &dyn HostExec,
    workspace: &str,
    saved: &SavedSession,
    force: bool,
) -> Result<Attached> {
    let recreated = check_layout_shells(exec, saved.layout(), force)?;
    let mut lock = ports.store.lock()?;
    refuse_i3mux_windows(&ports.sway.get_tree()?, workspace)?;
    take_and_bind(lock.state_mut(), ports, exec, workspace, saved.name())?;
    lock.commit()?;
    start_watcher(ports);
    open_layout(ports, exec, saved.layout(), force)?;
    Ok(Attached {
        session: saved.name().clone(),
        host: exec.host().clone(),
        total: saved.layout().terminals().len(),
        recreated,
        source: LayoutSource::Host,
    })
}

/// Refuse a layout with a shell that is attached elsewhere, unless `force`
/// (plan rule 10). Returns how many of its shells the host no longer has
/// (plan rule 11).
fn check_layout_shells(exec: &dyn HostExec, layout: &Layout, force: bool) -> Result<usize> {
    let shells = mux::list(exec)?;
    let find = |t: &TerminalName| shells.iter().find(|s| s.name() == t);
    let terminals = layout.terminals();
    if !force {
        if let Some(busy) = terminals
            .iter()
            .find(|t| find(t).is_some_and(Shell::is_attached))
        {
            return Err(CommandError::AlreadyAttached((*busy).clone()).into());
        }
    }
    Ok(terminals.iter().filter(|t| find(t).is_none()).count())
}

/// Refuse a `workspace` that already has i3mux windows (plan rule 6).
fn refuse_i3mux_windows(tree: &Value, workspace: &str) -> Result<(), CommandError> {
    if window::find_workspace(tree, workspace)
        .is_some_and(|ws| !window::i3mux_windows_in(ws).is_empty())
    {
        return Err(CommandError::WorkspaceHasTerminals(workspace.to_owned()));
    }
    Ok(())
}

/// Open the windows of `layout` and arrange them.
fn open_layout(ports: &Ports, exec: &dyn HostExec, layout: &Layout, force: bool) -> Result<()> {
    for step in layout::restore_plan(layout) {
        match step {
            Step::Launch(terminal) => open_window(ports, exec, &terminal, force, None)?,
            Step::Sway(command) => ports.sway.run(command)?,
        }
    }
    Ok(())
}

enum Pick {
    One(SessionName),
    Several(Vec<SessionName>),
}

/// The saved sessions of the host that are not bound on this machine.
fn pick_saved_session(ports: &Ports, exec: &dyn HostExec) -> Result<Pick> {
    let state = ports.store.load()?;
    let mut candidates: Vec<SessionName> = session::list(exec)?
        .into_iter()
        .filter(|s| state.workspace_of(exec.host(), s).is_none())
        .collect();
    match candidates.len() {
        0 => Err(CommandError::NoSavedSessions(exec.host().clone()).into()),
        1 => Ok(Pick::One(candidates.remove(0))),
        _ => Ok(Pick::Several(candidates)),
    }
}

#[derive(Debug)]
struct Killed {
    windows: usize,
    shells: usize,
    layout_deleted: bool,
}

/// End `session` on `host`, in this order: check that the host answers,
/// take the session, close the windows (before the shells, so no wrapper
/// shows an error), kill the shells, delete the saved layout, delete the
/// token, remove the binding and its backup. An unreachable host therefore
/// leaves every window open.
///
/// The take (plan-layout-backup rule 1) makes every other machine's binding
/// of the session stale at once, so their `detach` cannot save the layout
/// of a killed session again, even if this kill stops halfway. The token
/// delete then ends the session (rule 1b): other machines say so. A kill of
/// an unknown session also deletes the token it took, so it leaves no
/// token file. The backup goes with the binding (rule 16).
fn kill(ports: &Ports, host: &Host, name: &SessionName) -> Result<Killed> {
    let exec = (ports.hosts)(host);
    let mut lock = ports.store.lock()?;
    mux::list(exec.as_ref())?;
    session::take(exec.as_ref(), name)?;
    let windows = sway_ops::kill_all(ports.sway, host, name)?.len();
    let shells = mux::kill_session(exec.as_ref(), name)?.len();
    let layout_deleted = session::delete(exec.as_ref(), name)?;
    session::remove_token(exec.as_ref(), name)?;
    let unbound = lock.state_mut().unbind_session(host, name).is_some();
    if windows == 0 && shells == 0 && !layout_deleted && !unbound {
        return Err(CommandError::NoSuchSession {
            session: name.clone(),
            host: host.clone(),
        }
        .into());
    }
    lock.commit()?;
    ports.backups.remove(host, name)?;
    Ok(Killed {
        windows,
        shells,
        layout_deleted,
    })
}

/// One line of `i3mux sessions`.
#[derive(Debug, PartialEq, Eq)]
struct SessionRow {
    name: SessionName,
    host: Host,
    status: RowStatus,
}

#[derive(Debug, PartialEq, Eq)]
enum RowStatus {
    /// Bound to a workspace on this machine.
    Bound {
        workspace: String,
        windows: usize,
        orphans: usize,
    },
    /// Bound to a workspace on this machine, but its windows went away with
    /// an earlier Sway: `terminal` resumes it (plan-layout-backup
    /// decision 4). `backed_up` counts the terminals of its backup.
    Resumable {
        workspace: String,
        backed_up: Option<usize>,
    },
    /// Not bound here, but a terminal elsewhere shows at least one shell.
    AttachedElsewhere { shells: usize },
    /// Not bound here, no shell attached; a saved layout exists.
    Detached { saved: usize },
    /// Not bound here, no shell attached; the saved layout is corrupt.
    Unreadable,
    /// Not bound here, no shell attached, nothing saved, but shells run.
    Running { shells: usize },
}

/// The sessions of `host`: bound here, saved there, or with live shells.
/// A corrupt saved layout is one row, not an error for the whole list.
fn sessions(ports: &Ports, host: &Host) -> Result<Vec<SessionRow>> {
    let exec = (ports.hosts)(host);
    let shells = mux::list(exec.as_ref())?;
    let saved = session::list(exec.as_ref())?;
    let state = ports.store.load()?;
    let bound: Vec<(&str, &WorkspaceState)> =
        state.bindings().filter(|(_, b)| b.host() == host).collect();
    let tree = if bound.is_empty() {
        None
    } else {
        Some(ports.sway.get_tree()?)
    };

    let names: BTreeSet<&SessionName> = shells
        .iter()
        .map(|s| s.name().session())
        .chain(saved.iter())
        .chain(bound.iter().map(|(_, binding)| binding.session()))
        .collect();
    let mut rows = Vec::new();
    for name in names {
        let alive: Vec<&Shell> = shells
            .iter()
            .filter(|s| s.name().session() == name)
            .collect();
        let bound_here = bound.iter().find(|(_, b)| b.session() == name);
        let status = match (&tree, bound_here) {
            (Some(tree), Some((workspace, binding))) => {
                bound_status(ports, tree, binding, workspace, &alive)?
            }
            _ if alive.iter().any(|s| s.is_attached()) => RowStatus::AttachedElsewhere {
                shells: alive.len(),
            },
            _ if saved.contains(name) => saved_status(exec.as_ref(), name)?,
            _ => RowStatus::Running {
                shells: alive.len(),
            },
        };
        rows.push(SessionRow {
            name: name.clone(),
            host: host.clone(),
            status,
        });
    }
    Ok(rows)
}

/// Windows of a session bound here, and its shells without a window; or,
/// for a resumable binding, the size of its backup.
fn bound_status(
    ports: &Ports,
    tree: &Value,
    binding: &WorkspaceState,
    workspace: &str,
    alive: &[&Shell],
) -> Result<RowStatus> {
    if is_resumable(binding, ports.current_sway, tree) {
        return resumable_status(ports, binding, workspace);
    }
    let marked = window::session_terminals(tree, binding.host(), binding.session());
    let orphans = alive
        .iter()
        .filter(|s| !s.is_attached() && !marked.contains(s.name()))
        .count();
    Ok(RowStatus::Bound {
        workspace: workspace.to_owned(),
        windows: marked.len(),
        orphans,
    })
}

/// [`RowStatus::Resumable`], with the size of the binding's backup.
fn resumable_status(ports: &Ports, binding: &WorkspaceState, workspace: &str) -> Result<RowStatus> {
    let backed_up = ports.backups.read(binding)?;
    Ok(RowStatus::Resumable {
        workspace: workspace.to_owned(),
        backed_up: backed_up.map(|layout| layout.terminals().len()),
    })
}

/// What `sessions --all` found.
#[derive(Debug)]
struct Listing {
    rows: Vec<SessionRow>,
    /// Every host that was asked, in order.
    listed: Vec<Host>,
    /// Hosts that failed. Their sessions are missing from `rows`.
    failed: Vec<(Host, anyhow::Error)>,
}

/// The sessions of this machine, of every host a workspace is bound to,
/// and of `extra`. A host that fails is reported in [`Listing::failed`]
/// and does not hide the other hosts' sessions.
fn sessions_all(ports: &Ports, extra: Option<Host>) -> Result<Listing> {
    let state = ports.store.load()?;
    let mut listed = vec![Host::Local];
    for host in state.bindings().map(|(_, b)| b.host().clone()).chain(extra) {
        if !listed.contains(&host) {
            listed.push(host);
        }
    }
    let mut rows = Vec::new();
    let mut failed = Vec::new();
    for host in &listed {
        match sessions(ports, host) {
            Ok(host_rows) => rows.extend(host_rows),
            Err(err) => failed.push((host.clone(), err)),
        }
    }
    Ok(Listing {
        rows,
        listed,
        failed,
    })
}

/// What `i3mux status` shows.
#[derive(Debug)]
struct StatusReport {
    row: SessionRow,
    /// The state of a remote host's ssh master; `None` for this machine,
    /// and for a resumable binding whose host has no master.
    connection: Option<MasterState>,
}

/// The session bound to `workspace`, its window count, and the state of
/// its host's ssh master, from local state, Sway and systemd only, so it
/// answers fast even when the host is slow or down (plan-ssh-recovery
/// rule 13). Orphans need the host, so they are not counted.
fn status(ports: &Ports, workspace: &str) -> Result<Option<StatusReport>> {
    let Some(binding) = ports.store.load()?.get(workspace).cloned() else {
        return Ok(None);
    };
    let tree = ports.sway.get_tree()?;
    let resumable = is_resumable(&binding, ports.current_sway, &tree);
    let status = if resumable {
        resumable_status(ports, &binding, workspace)?
    } else {
        let windows = window::session_terminals(&tree, binding.host(), binding.session()).len();
        RowStatus::Bound {
            workspace: workspace.to_owned(),
            windows,
            orphans: 0,
        }
    };
    let connection = match binding.host() {
        Host::Local => None,
        Host::Ssh(remote) => Some(ports.masters.state(remote)?),
    };
    Ok(Some(StatusReport {
        row: SessionRow {
            name: binding.session().clone(),
            host: binding.host().clone(),
            status,
        },
        connection: connection.filter(|c| !resumable || c.has_master()),
    }))
}

/// The row, then `  resume: ...` for a resumable binding, then
/// `  connection: ...` for a remote host. The first line is a `sessions`
/// row; scripts/i3mux-rofi reads its first word, its host, and whether it
/// is resumable.
fn format_status(report: &StatusReport) -> Vec<String> {
    let mut lines = format_rows(std::slice::from_ref(&report.row));
    if matches!(report.row.status, RowStatus::Resumable { .. }) {
        lines.push("  resume: run 'i3mux terminal' here".to_owned());
    }
    if let Some(connection) = &report.connection {
        lines.push(format!("  connection: {connection}"));
    }
    lines
}

/// The saved layout's size, or [`RowStatus::Unreadable`] if it is corrupt.
fn saved_status(exec: &dyn HostExec, name: &SessionName) -> Result<RowStatus> {
    match session::load(exec, name) {
        Ok(saved) => Ok(RowStatus::Detached {
            saved: saved.map_or(0, |s| s.layout().terminals().len()),
        }),
        Err(SessionError::Corrupt { .. }) => Ok(RowStatus::Unreadable),
        Err(err) => Err(err.into()),
    }
}

/// `ws4  (deepthought)  workspace 4, 3 windows, 1 orphan`, or
/// `ws4  (deepthought)  workspace 4, resumable, 3 terminals backed up`. The
/// first word is the session name and the host is in parentheses;
/// scripts/i3mux-rofi relies on both, and on the text after the host of a
/// bound row.
fn format_rows(rows: &[SessionRow]) -> Vec<String> {
    let width = rows
        .iter()
        .map(|r| r.name.as_str().chars().count())
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|row| {
            let status = match &row.status {
                RowStatus::Bound {
                    workspace,
                    windows,
                    orphans,
                } if *orphans > 0 => format!(
                    "workspace {workspace}, {}, {}",
                    count(*windows, "window", "windows"),
                    count(*orphans, "orphan", "orphans")
                ),
                RowStatus::Bound {
                    workspace, windows, ..
                } => format!(
                    "workspace {workspace}, {}",
                    count(*windows, "window", "windows")
                ),
                RowStatus::Resumable {
                    workspace,
                    backed_up: Some(terminals),
                } => format!(
                    "workspace {workspace}, resumable, {} backed up",
                    count(*terminals, "terminal", "terminals")
                ),
                RowStatus::Resumable {
                    workspace,
                    backed_up: None,
                } => format!("workspace {workspace}, resumable, no backup"),
                RowStatus::AttachedElsewhere { shells } => {
                    format!("attached elsewhere, {}", count(*shells, "shell", "shells"))
                }
                RowStatus::Detached { saved } => {
                    format!("detached, {} saved", count(*saved, "terminal", "terminals"))
                }
                RowStatus::Unreadable => "unreadable saved layout".to_owned(),
                RowStatus::Running { shells } => {
                    format!("{}, no saved layout", count(*shells, "shell", "shells"))
                }
            };
            format!("{:<width$}  ({})  {status}", row.name.as_str(), row.host)
        })
        .collect()
}

#[cfg(test)]
mod handler_tests;
