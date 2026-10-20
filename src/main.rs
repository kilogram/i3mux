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
//! backup, without a take (rules 3 and 11-15).
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
use state::{LocalState, StateLock, StateStore, WorkspaceState};
use sway_ops::Sway;
use types::{Host, InvalidName, RemoteHost, SessionName, SwayInstance, TerminalName};
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
    #[error("Workspace {workspace} has no windows of session '{session}'; nothing to detach (to end the session, run 'i3mux kill')")]
    NothingToDetach {
        workspace: String,
        session: SessionName,
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
        log: Log {
            verbose: cli.verbose,
        },
        disconnect_wait: DISCONNECT_WAIT,
        backups: &backups,
        current_sway: &current_sway,
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
        TerminalOutcome::Restored(attached) => {
            for line in format_attached(&attached, workspace.name()) {
                println!("{line}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_detach(ports: &Ports, wm: &Wm) -> Result<ExitCode> {
    let workspace = wm.focused_workspace()?;
    let detached = detach(ports, workspace.name(), now_unix())?;
    println!("{}", format_detached(&detached));
    Ok(ExitCode::SUCCESS)
}

/// What `detach` prints. A session that another machine took since, or
/// that ended, is closed here without a save (plan-layout-backup rule 5),
/// and says so.
fn format_detached(detached: &Detached) -> String {
    let terminals = count(detached.terminals, "terminal", "terminals");
    let (session, host) = (&detached.session, &detached.host);
    match detached.token {
        TokenMatch::Same => {
            format!("✓ Detached session '{session}': layout of {terminals} saved on {host}")
        }
        TokenMatch::TakenElsewhere => format!(
            "✓ Closed session '{session}' here: another machine took it since, \
             so the layout of these {terminals} was not saved on {host}"
        ),
        TokenMatch::Ended => format!(
            "✓ Closed session '{session}' here: it ended on {host}, \
             so the layout of these {terminals} was not saved"
        ),
    }
}

fn cmd_attach(
    ports: &Ports,
    wm: &Wm,
    remote: Option<String>,
    session: Option<String>,
    force: bool,
) -> Result<ExitCode> {
    let host = Host::from_remote_arg(remote)?;
    let session = parse_session(session)?;
    let workspace = wm.focused_workspace()?;
    match attach(ports, &host, workspace.name(), session, force)? {
        AttachOutcome::Choose(candidates) => {
            eprintln!("Several saved sessions on {host}:");
            for name in &candidates {
                eprintln!("  {name}");
            }
            eprintln!("Choose one with --session NAME.");
            Ok(ExitCode::from(EXIT_CHOOSE))
        }
        AttachOutcome::Attached(attached) => {
            for line in format_attached(&attached, workspace.name()) {
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// What `attach`, or a `terminal` that brought windows back, prints.
fn format_attached(attached: &Attached, workspace: &str) -> Vec<String> {
    let Attached {
        session,
        host,
        total,
        recreated,
        source,
    } = attached;
    let terminals = count(*total, "terminal", "terminals");
    let attached = || {
        format!("✓ Attached session '{session}' from {host} to workspace {workspace} ({terminals})")
    };
    let mut lines = match source {
        LayoutSource::Host => vec![attached()],
        LayoutSource::HostAfterLoss(lost) => vec![lost_notice(session, host, *lost), attached()],
        LayoutSource::Backup => vec![format!(
            "✓ Resumed session '{session}' on {host} in workspace {workspace} ({terminals})"
        )],
    };
    if *recreated > 0 {
        lines.push(format!(
            "  recreated {recreated} of {total} shells: the host had lost them, for example after a reboot"
        ));
    }
    lines
}

/// Why a resume dropped its binding (plan-layout-backup rules 13 and 13a).
fn lost_notice(session: &SessionName, host: &Host, lost: Lost) -> String {
    match lost {
        Lost::TakenElsewhere => {
            format!("{session} was taken by another machine since; using the layout saved there.")
        }
        Lost::Ended => format!("{session} ended on {host}."),
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
        take_and_bind(
            state,
            exec.as_ref(),
            workspace.name(),
            &session,
            ports.current_sway,
        )
    })?;
    let choice = open_terminal(ports, workspace.name(), &TerminalRequest::default())?;
    Ok((session, choice))
}

/// Refuse a bind that guarantee 1 of `state` forbids, else take `session`
/// on its host and bind `workspace` to it with the new token, owned by
/// `owner`. The caller holds the state lock and commits (plan-layout-backup
/// rule 2). A resume never calls this (rule 3).
///
/// If the take fails, `state` is unchanged. If the commit fails after the
/// take, no binding holds the new token, and the next take replaces it.
fn take_and_bind(
    state: &mut LocalState,
    exec: &dyn HostExec,
    workspace: &str,
    session: &SessionName,
    owner: &SwayInstance,
) -> Result<()> {
    state.check_bind(workspace, exec.host(), session)?;
    let token = session::take(exec, session)?;
    let binding = WorkspaceState::new(exec.host().clone(), session.clone(), token, owner.clone());
    state.bind(workspace, binding)?;
    Ok(())
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
        let owned = binding.with_owner(ports.current_sway.clone());
        lock.state_mut().bind(workspace, owned)?;
        lock.commit()?;
    }
    Ok(choice)
}

/// What `terminal` did in a bound workspace.
#[derive(Debug)]
enum TerminalOutcome {
    /// It opened one window (plan rule 4).
    Opened(Choice),
    /// It brought the windows of a resumable binding back
    /// (plan-layout-backup rule 11).
    Restored(Attached),
}

/// Open a terminal in the bound `workspace`, or, if its binding is
/// resumable, resume it instead (plan-layout-backup rule 11).
///
/// With no backup (rule 15), it restores the layout saved on the host, or,
/// if there is none, opens one terminal as for a binding of this Sway. Both
/// reattach with force: the host token is still the binding's, so no other
/// machine took the session since.
fn terminal(ports: &Ports, workspace: &str, request: &TerminalRequest) -> Result<TerminalOutcome> {
    let binding = match resume(ports, workspace, request.force, |_| true)? {
        Resume::NotResumable => {
            return open_terminal(ports, workspace, request).map(TerminalOutcome::Opened);
        }
        Resume::Restored(attached) => return Ok(TerminalOutcome::Restored(attached)),
        Resume::NoBackup(binding) => binding,
    };
    let exec = (ports.hosts)(binding.host());
    if let Some(saved) = session::load(exec.as_ref(), binding.session())? {
        return attach_saved(ports, exec.as_ref(), workspace, &saved, true)
            .map(TerminalOutcome::Restored);
    }
    let forced = TerminalRequest {
        command: request.command.clone(),
        new: request.new,
        force: true,
    };
    open_terminal(ports, workspace, &forced).map(TerminalOutcome::Opened)
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
    Ok(Attached {
        session: binding.session().clone(),
        host: binding.host().clone(),
        total: layout.terminals().len(),
        recreated,
        source: LayoutSource::Backup,
    })
}

/// After the binding of `workspace` was `lost` and dropped: attach the
/// layout saved on the host (plan-layout-backup rules 13 and 13a). An error
/// says why the workspace is no longer bound.
fn attach_lost_binding(
    ports: &Ports,
    binding: &WorkspaceState,
    workspace: &str,
    force: bool,
    lost: Lost,
) -> Result<Attached> {
    let attached = attach_named(ports, binding.host(), workspace, binding.session(), force)
        .with_context(|| {
            format!(
                "{} Workspace {workspace} is no longer bound to it",
                lost_notice(binding.session(), binding.host(), lost)
            )
        })?;
    Ok(Attached {
        source: LayoutSource::HostAfterLoss(lost),
        ..attached
    })
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
    let layout = window::find_workspace(&tree, workspace)
        .and_then(|ws| layout::capture(ws, host, name))
        .ok_or_else(|| CommandError::NothingToDetach {
            workspace: workspace.to_owned(),
            session: name.clone(),
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

/// Where the layout that `attach` or `terminal` restored came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutSource {
    /// The layout saved on the host.
    Host,
    /// The layout saved on the host, after the workspace's binding was lost
    /// (plan-layout-backup rules 13 and 13a).
    HostAfterLoss(Lost),
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

/// Restore a session of `host` into `workspace`.
///
/// If the workspace has a resumable binding on `host`, and `requested`
/// names its session or none, `attach` resumes it (plan-layout-backup
/// rule 11). Without a backup it restores that session's host layout, with
/// force, as [`terminal`] does (rule 15). Otherwise it restores a saved
/// session, as [`attach_from_host`].
fn attach(
    ports: &Ports,
    host: &Host,
    workspace: &str,
    requested: Option<SessionName>,
    force: bool,
) -> Result<AttachOutcome> {
    let wanted = |binding: &WorkspaceState| {
        binding.host() == host && requested.as_ref().is_none_or(|s| s == binding.session())
    };
    match resume(ports, workspace, force, wanted)? {
        Resume::Restored(attached) => Ok(AttachOutcome::Attached(attached)),
        Resume::NoBackup(binding) => attach_named(ports, host, workspace, binding.session(), true)
            .map(AttachOutcome::Attached),
        Resume::NotResumable => attach_from_host(ports, host, workspace, requested, force),
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
    take_and_bind(
        lock.state_mut(),
        exec,
        workspace,
        saved.name(),
        ports.current_sway,
    )?;
    lock.commit()?;
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
    let bound: Vec<(&str, &SessionName)> = state
        .bindings()
        .filter(|(_, b)| b.host() == host)
        .map(|(ws, b)| (ws, b.session()))
        .collect();
    let tree = if bound.is_empty() {
        None
    } else {
        Some(ports.sway.get_tree()?)
    };

    let names: BTreeSet<&SessionName> = shells
        .iter()
        .map(|s| s.name().session())
        .chain(saved.iter())
        .chain(bound.iter().map(|(_, name)| *name))
        .collect();
    let mut rows = Vec::new();
    for name in names {
        let alive: Vec<&Shell> = shells
            .iter()
            .filter(|s| s.name().session() == name)
            .collect();
        let workspace = bound.iter().find(|(_, b)| *b == name).map(|(ws, _)| *ws);
        let status = match (&tree, workspace) {
            (Some(tree), Some(workspace)) => bound_status(tree, host, name, workspace, &alive),
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

/// Windows of a session bound here, and its shells without a window.
fn bound_status(
    tree: &Value,
    host: &Host,
    name: &SessionName,
    workspace: &str,
    alive: &[&Shell],
) -> RowStatus {
    let marked = window::session_terminals(tree, host, name);
    let orphans = alive
        .iter()
        .filter(|s| !s.is_attached() && !marked.contains(s.name()))
        .count();
    RowStatus::Bound {
        workspace: workspace.to_owned(),
        windows: marked.len(),
        orphans,
    }
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
    /// The state of a remote host's ssh master; `None` for this machine.
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
    let windows = window::session_terminals(&tree, binding.host(), binding.session()).len();
    let connection = match binding.host() {
        Host::Local => None,
        Host::Ssh(remote) => Some(ports.masters.state(remote)?),
    };
    Ok(Some(StatusReport {
        row: SessionRow {
            name: binding.session().clone(),
            host: binding.host().clone(),
            status: RowStatus::Bound {
                workspace: workspace.to_owned(),
                windows,
                orphans: 0,
            },
        },
        connection,
    }))
}

/// The row, then `  connection: ...` for a remote host. The first line is
/// a `sessions` row; scripts/i3mux-rofi reads its first word and host.
fn format_status(report: &StatusReport) -> Vec<String> {
    let mut lines = format_rows(std::slice::from_ref(&report.row));
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

/// `ws4  (deepthought)  workspace 4, 3 windows, 1 orphan`. The first word is
/// the session name and the host is in parentheses; scripts/i3mux-rofi
/// relies on both.
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
