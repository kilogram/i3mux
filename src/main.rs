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
//! Every command, on success and on failure, ends in
//! [`with_unused_masters_stopped`]: it stops the ssh master of each host the
//! command touched that no workspace is bound to and no window is marked
//! with (plan-ssh-recovery rule 12; PLAN-terminal-here decision 5). Nothing
//! runs on a host after that stop.

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

use host::{HostExec, HostRunner, RunnerConfig};
use launch::{Launcher, RemoteAttach, TerminalLauncher, WindowCommand, WrapperOptions};
use layout::Step;
use master::{DirectSsh, Link, MasterState, Masters, SystemdMasters, TrackedMasters};
use mux::{Choice, Shell};
use serde_json::Value;
use session::{SavedSession, SessionError};
use state::{StateStore, WorkspaceState};
use sway_ops::Sway;
use types::{Host, InvalidName, RemoteHost, SessionName, TerminalName, WindowTemplate};
use window::Mark;
use wm::{Wm, WorkspaceInfo};

const FORCE_HELP: &str = "Disconnect any other client attached to these shells";

/// Exit status of `attach` when several sessions fit and none was named.
const EXIT_CHOOSE: u8 = 2;

/// Exit status of `status` when the focused workspace is not bound.
const EXIT_UNBOUND: u8 = 3;

/// Exit status of any command refused because the shell it names does
/// not exist on its host (README, "Exit status").
const EXIT_NO_SHELL: u8 = 4;

/// Exit status of any command refused because no workspace is bound to
/// the session it names (README, "Exit status").
const EXIT_NO_BINDING: u8 = 5;

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
        /// Each window of this workspace runs CMD instead of i3mux's own
        /// attach; {session} and {shell} in CMD become the session and the
        /// shell names
        #[arg(long, value_name = "CMD")]
        window_command: Option<String>,
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
        /// Close the window as soon as its shell ends or is detached, with
        /// the shell's exit status, instead of keeping it open with a hint
        #[arg(long)]
        no_hold: bool,
        /// Make the terminal this command runs in a window of its
        /// workspace's session, instead of opening one: mark it, then run
        /// the shell (or the window command) in it
        #[arg(long)]
        here: bool,
        /// With --here: attach this existing shell of the session, {session}-{NNN};
        /// it never starts one, and the window command does not run. A shell
        /// that another window shows needs --force, which moves it here
        #[arg(long, value_name = "NAME", requires = "here", conflicts_with_all = ["exec", "new"])]
        shell: Option<String>,
        /// With --here: the host of the session; an unbound workspace is bound
        /// to it, and a workspace bound to another host is refused
        /// [default: the binding's host, or this machine]
        #[arg(short, long, requires = "here")]
        remote: Option<String>,
        /// With --here: the window command of an unbound workspace's new binding
        /// (see activate); a workspace bound with another one is refused
        #[arg(long, value_name = "CMD", requires = "here")]
        window_command: Option<String>,
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
        /// Only move the binding of --session to the --remote host and
        /// re-mark its windows: open, close and attach nothing
        #[arg(long, requires = "session", conflicts_with = "force")]
        rebind_only: bool,
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
    /// Open a window on an existing shell, in the workspace bound to its session
    Open {
        /// The host that runs the shell [default: this machine]
        #[arg(short, long)]
        remote: Option<String>,
        #[arg(long, help = FORCE_HELP)]
        force: bool,
        /// The shell: {session}-{NNN}, for example ws4-003
        shell: String,
    },
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
    #[error("Shell {0} already has a window; use --force to move it to this terminal")]
    ShownElsewhere(TerminalName),
    #[error("No saved session '{session}' on {host}; run 'i3mux sessions' to list sessions")]
    NoSavedSession { session: SessionName, host: Host },
    #[error("No saved sessions on {0} that are not already bound here")]
    NoSavedSessions(Host),
    #[error("No session '{session}' on {host}")]
    NoSuchSession { session: SessionName, host: Host },
    #[error("No workspace is bound to session '{0}'; nothing to rebind")]
    NothingToRebind(SessionName),
    #[error(
        "Workspace {workspace} is bound to session '{session}' on {bound}, not {asked}; \
         nothing changed"
    )]
    BoundToOtherHost {
        workspace: String,
        session: SessionName,
        bound: Host,
        asked: Host,
    },
    #[error("Workspace {0} is bound with another window command; nothing changed")]
    OtherWindowCommand(String),
    #[error(
        "Shell {shell} is not a shell of session '{session}', which workspace {workspace} \
         is bound to"
    )]
    ShellOfOtherSession {
        shell: TerminalName,
        session: SessionName,
        workspace: String,
    },
    #[error("This terminal's window is on no workspace")]
    NoWorkspace,
    #[error(
        "Session '{session}' is bound in workspaces {workspaces} (on different hosts); \
         rebind needs one: end the other with 'i3mux kill --remote HOST --session {session}'"
    )]
    BoundTwice {
        session: SessionName,
        workspaces: String,
    },
    #[error(
        "No workspace is bound to session '{session}' on {host}; run 'i3mux activate{} --session {session}' in the workspace that should show it",
        remote_flag(.host)
    )]
    NotBound { session: SessionName, host: Host },
    #[error("No shell '{terminal}' on {host}; run 'i3mux sessions{}' to list sessions", remote_flag(.host))]
    NoSuchShell { terminal: TerminalName, host: Host },
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
            ExitCode::from(failure_status(&err))
        }
    }
}

/// The exit status of a command that failed with `err`: a missing shell
/// and a missing binding have their own, so a caller can branch on them
/// without reading the message; any other failure is 1.
fn failure_status(err: &anyhow::Error) -> u8 {
    match err.chain().find_map(|e| e.downcast_ref::<CommandError>()) {
        Some(CommandError::NoSuchShell { .. }) => EXIT_NO_SHELL,
        Some(CommandError::NotBound { .. } | CommandError::NothingToRebind(_)) => EXIT_NO_BINDING,
        _ => 1,
    }
}

/// How a command ends: with an exit status, or (`terminal --here`) by
/// running another program in this process.
#[derive(Debug)]
enum Outcome {
    Exit(ExitCode),
    Exec(Box<HereExec>),
}

/// Wire the real ports, run the command, then stop the masters it left
/// unused. Only then does `terminal --here` exec its window's program, so
/// nothing of i3mux runs after the stop.
fn run(cli: Cli) -> Result<ExitCode> {
    let wm = Wm::connect()?;
    let store = StateStore::from_env()?;
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
    };
    let outcome = with_unused_masters_stopped(&store, &masters, &wm, || {
        dispatch(&ports, &wm, &launcher, cli.command)
    })?;
    let here = match outcome {
        Outcome::Exit(code) => return Ok(code),
        Outcome::Exec(here) => here,
    };
    let failed = exec_program(&here.argv);
    let undone =
        with_unused_masters_stopped(&store, &masters, &wm, || undo_here(&ports, &here.changes));
    Err(match undone {
        Ok(()) => anyhow::anyhow!("{failed:#}; i3mux undid its changes"),
        Err(err) => anyhow::anyhow!("{failed:#}; undoing i3mux's changes failed: {err:#}"),
    })
}

/// Run `argv` in this process, in place of i3mux. Returns only on failure.
fn exec_program(argv: &[String]) -> anyhow::Error {
    use std::os::unix::process::CommandExt as _;
    let Some((program, arguments)) = argv.split_first() else {
        return anyhow::anyhow!("i3mux bug: an empty window command");
    };
    let err = std::process::Command::new(program).args(arguments).exec();
    anyhow::Error::new(err).context(format!("cannot run '{program}' in this terminal"))
}

/// XTWINOPS: save the terminal's title on its title stack.
const TITLE_PUSH: &str = "\x1b[22;0t";
/// XTWINOPS: restore the title saved last.
const TITLE_POP: &str = "\x1b[23;0t";

/// Set the title of the terminal of this process (OSC 0).
fn set_terminal_title(title: &str) -> Result<()> {
    write_tty(&format!("\x1b]0;{title}\x07"))
}

/// Write `text` to the terminal of this process (`/dev/tty`).
fn write_tty(text: &str) -> Result<()> {
    use std::io::Write as _;
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .context(
            "'i3mux terminal --here' runs in a terminal, and this process has none (/dev/tty)",
        )?;
    tty.write_all(text.as_bytes())
        .and_then(|()| tty.flush())
        .context("cannot write to /dev/tty")
}

fn dispatch(
    ports: &Ports,
    wm: &Wm,
    launcher: &TerminalLauncher,
    command: CliCommand,
) -> Result<Outcome> {
    if let CliCommand::Terminal {
        exec,
        new,
        force,
        no_hold,
        here: true,
        shell,
        remote,
        window_command,
    } = command
    {
        let request = HereRequest {
            host: remote.map(RemoteHost::new).transpose()?.map(Host::Ssh),
            shell: shell.map(|s| s.parse()).transpose()?,
            terminal: TerminalRequest {
                command: exec,
                new,
                force,
                no_hold,
            },
            window_command: window_command.map(WindowTemplate::new).transpose()?,
        };
        return cmd_terminal_here(ports, &request).map(|here| Outcome::Exec(Box::new(here)));
    }
    dispatch_exit(ports, wm, launcher, command).map(Outcome::Exit)
}

fn dispatch_exit(
    ports: &Ports,
    wm: &Wm,
    launcher: &TerminalLauncher,
    command: CliCommand,
) -> Result<ExitCode> {
    match command {
        CliCommand::Activate {
            remote,
            session,
            window_command,
        } => cmd_activate(ports, wm, remote, session, window_command),
        CliCommand::Terminal {
            exec,
            new,
            force,
            no_hold,
            ..
        } => {
            let request = TerminalRequest {
                command: exec,
                new,
                force,
                no_hold,
            };
            cmd_terminal(ports, wm, launcher, &request)
        }
        CliCommand::Detach => cmd_detach(ports, wm),
        CliCommand::Attach {
            remote,
            session,
            force,
            rebind_only: false,
        } => cmd_attach(ports, wm, remote, session, force),
        CliCommand::Attach {
            remote,
            session,
            rebind_only: true,
            ..
        } => cmd_rebind(ports, remote, session),
        CliCommand::Sessions { remote, all } => cmd_sessions(ports, remote, all),
        CliCommand::Status => cmd_status(ports, wm),
        CliCommand::Open {
            remote,
            force,
            shell,
        } => cmd_open(ports, remote, &shell, force),
        CliCommand::Kill { remote, session } => cmd_kill(ports, wm, remote, session),
    }
}

/// Run `command`, then stop the ssh master of each host it touched that no
/// workspace is bound to and no window is marked with (plan-ssh-recovery
/// rule 12; PLAN-terminal-here decision 5), also when it failed. So a
/// failed `activate` or `attach`, or `sessions --remote`, leaves no master
/// behind, while a window that still shows a shell of a host, bound or not,
/// keeps its connection. Hosts the command did not touch keep their masters.
///
/// The stop runs after the command's last remote command, and under the
/// state lock, so no concurrent command binds the host between the check and
/// the stop. A command that touched no host takes no lock. The command's
/// error wins over a failed stop, which is then printed as a warning.
fn with_unused_masters_stopped<T>(
    store: &StateStore,
    masters: &TrackedMasters,
    sway: &dyn Sway,
    command: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let result = command();
    match (result, stop_unused_masters(store, masters, sway)) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(stop_err)) => Err(stop_err),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(stop_err)) => {
            eprintln!("i3mux: warning: {stop_err:#}");
            Err(err)
        }
    }
}

/// Stop the masters of the touched hosts that no binding uses and no
/// window is marked with. The Sway tree is read only when a touched host is
/// unbound.
fn stop_unused_masters(
    store: &StateStore,
    masters: &TrackedMasters,
    sway: &dyn Sway,
) -> Result<()> {
    let touched = masters.touched_hosts();
    if touched.is_empty() {
        return Ok(());
    }
    let lock = store.lock()?;
    let unbound: Vec<RemoteHost> = touched
        .into_iter()
        .filter(|remote| {
            let host = Host::Ssh(remote.clone());
            lock.state().bindings().all(|(_, b)| b.host() != &host)
        })
        .collect();
    if unbound.is_empty() {
        return Ok(());
    }
    let marked = window::hosts_with_windows(&sway.get_tree()?);
    for remote in unbound {
        if !marked.contains(&Host::Ssh(remote.clone())) {
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

/// ` --remote HOST` for a remote host, else nothing: the flag a message
/// tells the user to pass.
fn remote_flag(host: &Host) -> String {
    match host {
        Host::Local => String::new(),
        Host::Ssh(remote) => format!(" --remote {remote}"),
    }
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
    window_command: Option<String>,
) -> Result<ExitCode> {
    let host = Host::from_remote_arg(remote)?;
    let session = parse_session(session)?;
    let window_command = window_command.map(WindowTemplate::new).transpose()?;
    let workspace = wm.focused_workspace()?;
    let (session, choice) = activate(ports, &host, &workspace, session, window_command)?;
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
    } else {
        let choice = open_terminal(ports, workspace.name(), request)?;
        ports.log.debug(|| describe_choice(&choice));
    }
    Ok(ExitCode::SUCCESS)
}

/// `terminal --here`: set a nonce title on this terminal, find the window
/// that took it, and make it a window of its workspace's session. The
/// terminal's own title comes back afterwards (the title stack), also when
/// `here` refuses.
fn cmd_terminal_here(ports: &Ports, request: &HereRequest) -> Result<HereExec> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let nonce = format!("i3mux-here-{}-{nanos}", std::process::id());
    write_tty(TITLE_PUSH)?;
    set_terminal_title(&nonce)?;
    let found =
        sway_ops::wait_for_title(ports.sway, &nonce).and_then(|id| here(ports, id, request));
    let popped = write_tty(TITLE_POP);
    let here = found?;
    popped?;
    ports
        .log
        .debug(|| format!("running here: {}", shell_words::join(&here.argv)));
    Ok(here)
}

fn cmd_detach(ports: &Ports, wm: &Wm) -> Result<ExitCode> {
    let workspace = wm.focused_workspace()?;
    let detached = detach(ports, workspace.name(), now_unix())?;
    println!(
        "✓ Detached session '{}': layout of {} saved on {}",
        detached.session,
        count(detached.terminals, "terminal", "terminals"),
        detached.host
    );
    Ok(ExitCode::SUCCESS)
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
        AttachOutcome::Attached(a) => {
            println!(
                "✓ Attached session '{}' from {} to workspace {} ({})",
                a.session,
                host,
                workspace.name(),
                count(a.total, "terminal", "terminals")
            );
            if a.recreated > 0 {
                println!(
                    "  recreated {} of {} shells: the host had lost them, for example after a reboot",
                    a.recreated, a.total
                );
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn cmd_rebind(ports: &Ports, remote: Option<String>, session: Option<String>) -> Result<ExitCode> {
    let host = Host::from_remote_arg(remote)?;
    let session = parse_session(session)?
        .context("i3mux bug: clap lets --rebind-only through without --session")?;
    let rebound = rebind(ports, &host, &session)?;
    let moved = rebound
        .from
        .map_or_else(|| "already".to_owned(), |from| format!("moved from {from}"));
    println!(
        "✓ Workspace {}: session '{session}' {moved} on {host}; {} re-marked",
        rebound.workspace,
        count(rebound.remarked, "window", "windows")
    );
    Ok(ExitCode::SUCCESS)
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

fn cmd_open(ports: &Ports, remote: Option<String>, shell: &str, force: bool) -> Result<ExitCode> {
    let host = Host::from_remote_arg(remote)?;
    let terminal: TerminalName = shell.parse()?;
    match open(ports, &host, &terminal, force)? {
        Opened::Window(workspace) => {
            println!("✓ Opened shell {terminal} of {host} in workspace {workspace}");
        }
        Opened::AlreadyOpen(workspace) => {
            println!("✓ Shell {terminal} of {host} already has a window in workspace {workspace}");
        }
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
    /// End the window when its shell ends, with no pause.
    no_hold: bool,
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
            InvalidName::Host { .. }
            | InvalidName::Terminal(_)
            | InvalidName::WindowCommand { .. } => "invalid name",
        },
    })
}

/// Bind `workspace` to `session` on `host`, with `window_command` for its
/// windows, then open its first terminal. The host is checked (reachable,
/// shpool installed) before anything is bound.
fn activate(
    ports: &Ports,
    host: &Host,
    workspace: &WorkspaceInfo,
    session: Option<SessionName>,
    window_command: Option<WindowTemplate>,
) -> Result<(SessionName, Choice)> {
    let session = match session {
        Some(session) => session,
        None => default_session(workspace)?,
    };
    mux::list((ports.hosts)(host).as_ref())?;
    let binding =
        WorkspaceState::new(host.clone(), session.clone()).with_window_command(window_command);
    ports
        .store
        .with_lock(|state| Ok(state.bind(workspace.name(), binding)?))?;
    let choice = open_terminal(ports, workspace.name(), &TerminalRequest::default())?;
    Ok((session, choice))
}

/// The shells of `binding`'s session on its host.
fn session_shells(exec: &dyn HostExec, binding: &WorkspaceState) -> Result<Vec<Shell>> {
    Ok(mux::list(exec)?
        .into_iter()
        .filter(|s| s.name().session() == binding.session())
        .collect())
}

/// The shell a new window of `binding` shows (plan rule 4): the lowest
/// orphan, or a new number. A shell with a window in `tree` is never
/// chosen again, nor one attached elsewhere unless `request.force`.
fn pick_terminal(
    tree: &Value,
    binding: &WorkspaceState,
    shells: &[Shell],
    request: &TerminalRequest,
) -> Choice {
    let mut in_use = window::session_terminals(tree, binding.host(), binding.session());
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
    mux::choose_terminal(binding.session(), &alive, &in_use, force_new)
}

/// Open one window of the session bound to `workspace` (plan rule 4).
///
/// Under the state lock: list the host's shells, read the marked windows,
/// choose (lowest orphan, or a new number), spawn, wait for the window,
/// mark it. The mark is visible before the lock is released, so a
/// concurrent call never picks the same shell.
fn open_terminal(ports: &Ports, workspace: &str, request: &TerminalRequest) -> Result<Choice> {
    let lock = ports.store.lock()?;
    let binding = lock
        .state()
        .get(workspace)
        .cloned()
        .ok_or_else(|| CommandError::Unbound(workspace.to_owned()))?;
    let host = (ports.hosts)(binding.host());
    let shells = session_shells(host.as_ref(), &binding)?;
    let choice = pick_terminal(&ports.sway.get_tree()?, &binding, &shells, request);
    open_window(
        ports,
        host.as_ref(),
        choice.name(),
        &WindowSpec {
            force: request.force,
            command: request.command.as_deref(),
            options: WrapperOptions {
                no_hold: request.no_hold,
                attach_only: false,
            },
            template: binding.window_command(),
            exists: matches!(choice, Choice::Reattach(_)),
        },
    )?;
    drop(lock);
    Ok(choice)
}

/// A terminal to make a window of its workspace's session (`terminal
/// --here`, PLAN-terminal-here decision 1).
#[derive(Debug, Default)]
struct HereRequest {
    /// `--remote`: the host. An unbound workspace is bound to it, and a
    /// workspace bound to another host is refused. `None`: the binding's
    /// host, or this machine for an unbound workspace.
    host: Option<Host>,
    /// `--shell`: attach this existing shell, without the window command.
    shell: Option<TerminalName>,
    /// `-e`, `--new`, `--force`, `--no-hold`.
    terminal: TerminalRequest,
    /// `--window-command`: the window command of the binding it makes.
    window_command: Option<WindowTemplate>,
}

/// What `terminal --here` runs in its terminal, in place of i3mux.
#[derive(Debug, PartialEq, Eq)]
struct HereExec {
    /// `i3mux:{host}:{shell}`: the title the wrapper sets. i3mux sets no
    /// title itself, so a window command keeps the terminal's own.
    title: String,
    argv: Vec<String>,
    /// What `here` changed, for [`undo_here`] when the exec fails.
    changes: HereChanges,
}

/// What `here` changed in the state, on the host and in Sway.
#[derive(Debug, PartialEq, Eq)]
struct HereChanges {
    workspace: String,
    /// The binding `here` made; `None` when the workspace was bound before.
    bound: Option<WorkspaceState>,
    /// The shell `here` made for the window command, on its host.
    made: Option<(Host, TerminalName)>,
    marks: MarkChanges,
}

/// The i3mux marks a command added to and removed from windows, by
/// container id.
#[derive(Debug, Default, PartialEq, Eq)]
struct MarkChanges {
    added: Vec<(u64, Mark)>,
    removed: Vec<(u64, Mark)>,
}

/// Make window `con_id`, the terminal of this process, a window of its
/// workspace's session, and say what to run in it (PLAN-terminal-here
/// decision 1).
///
/// Every refusal comes before anything changes, so a caller can fall back:
/// a workspace bound to another host than `request.host`, or with another
/// window command; a `--shell` of another session, missing, or shown in
/// another window or attached elsewhere (both without `force`); an
/// unreachable host. Then, under the state lock, it makes a new shell for
/// the window command, or starts the host's master for the wrapper. Only
/// then does it bind an unbound workspace and mark the window with its
/// shell, replacing any other i3mux mark the window had. With `force`, the
/// other window of a `--shell` loses its mark. What it returns runs the
/// wrapper (with `--attach-only` for `--shell`), or the window command,
/// and says what changed, so that [`undo_here`] can undo it.
fn here(ports: &Ports, con_id: u64, request: &HereRequest) -> Result<HereExec> {
    let mut lock = ports.store.lock()?;
    let tree = ports.sway.get_tree()?;
    let node = window::workspace_containing(&tree, con_id).ok_or(CommandError::NoWorkspace)?;
    let workspace: WorkspaceInfo = serde_json::from_value(node.clone())
        .context("cannot read the workspace of this terminal's window")?;
    let name = workspace.name().to_owned();
    let bound = lock.state().get(&name).cloned();
    let binding = match &bound {
        Some(bound) => here_fits(&name, bound, request).map(|()| bound.clone())?,
        None => WorkspaceState::new(
            request.host.clone().unwrap_or(Host::Local),
            default_session(&workspace)?,
        )
        .with_window_command(request.window_command.clone()),
    };
    let exec = (ports.hosts)(binding.host());
    let shells = session_shells(exec.as_ref(), &binding)?;
    let (choice, elsewhere) = match &request.shell {
        Some(shell) => {
            let mark = Mark::new(binding.host().clone(), shell.clone());
            let elsewhere = other_windows_with(&tree, con_id, &mark);
            let shown = !elsewhere.is_empty();
            here_shell(
                &name,
                &binding,
                &shells,
                shell,
                shown,
                request.terminal.force,
            )?;
            (Choice::Reattach(shell.clone()), elsewhere)
        }
        None => (
            pick_terminal(&tree, &binding, &shells, &request.terminal),
            Vec::new(),
        ),
    };
    let spec = WindowSpec {
        force: request.terminal.force,
        command: request.terminal.command.as_deref(),
        options: WrapperOptions {
            no_hold: request.terminal.no_hold,
            attach_only: request.shell.is_some(),
        },
        template: binding.window_command().filter(|_| request.shell.is_none()),
        exists: !matches!(choice, Choice::Create(_)),
    };
    let window = window_program(ports, exec.as_ref(), choice.name(), &spec)?;
    let made = (spec.template.is_some() && !spec.exists)
        .then(|| (binding.host().clone(), choice.name().clone()));
    if bound.is_none() {
        lock.state_mut().bind(&name, binding.clone())?;
        lock.commit()?;
    }
    let mark = Mark::new(binding.host().clone(), choice.name().clone());
    let marks = remark(ports, &tree, con_id, &mark, &elsewhere)?;
    let title = format!("i3mux:{}:{}", mark.host(), mark.terminal());
    let argv = ports.launcher.program_argv(&title, &window, spec.options);
    let changes = HereChanges {
        workspace: name,
        bound: bound.is_none().then_some(binding),
        made,
        marks,
    };
    Ok(HereExec {
        title,
        argv,
        changes,
    })
}

/// Undo what `here` changed, after its exec failed: the marks it added go
/// and the ones it removed come back, on windows that still exist; the
/// shell it made ends; the binding it made goes, if no other command
/// changed it since.
///
/// The shell ends even though shpool may list it as attached: a shell made
/// with `attach -b` lists so for a moment. Nothing else attached it: until
/// this runs, under the state lock, its mark keeps every i3mux command
/// from picking it.
fn undo_here(ports: &Ports, changes: &HereChanges) -> Result<()> {
    let mut lock = ports.store.lock()?;
    let tree = ports.sway.get_tree()?;
    for (id, mark) in &changes.marks.added {
        if window::marks_of(&tree, *id).contains(mark) {
            ports.sway.run(&window::unmark_command(*id, mark))?;
        }
    }
    for (id, mark) in &changes.marks.removed {
        let exists = window::workspace_containing(&tree, *id).is_some();
        if exists && !window::marks_of(&tree, *id).contains(mark) {
            ports.sway.run(&window::mark_command(*id, mark))?;
        }
    }
    if let Some((host, shell)) = &changes.made {
        mux::kill((ports.hosts)(host).as_ref(), std::slice::from_ref(shell))?;
    }
    if let Some(bound) = &changes.bound {
        if lock.state().get(&changes.workspace) == Some(bound) {
            lock.state_mut().unbind(&changes.workspace);
            lock.commit()?;
        }
    }
    Ok(())
}

/// The windows other than `con_id` that carry `mark`.
fn other_windows_with(tree: &Value, con_id: u64, mark: &Mark) -> Vec<u64> {
    window::i3mux_windows_in(tree)
        .into_iter()
        .filter(|(id, m)| *id != con_id && m == mark)
        .map(|(id, _)| id)
        .collect()
}

/// Refuse `terminal --here` in a workspace bound other than `request`
/// asks: to another host, or with another window command.
fn here_fits(workspace: &str, bound: &WorkspaceState, request: &HereRequest) -> Result<()> {
    if let Some(asked) = request.host.as_ref().filter(|h| *h != bound.host()) {
        return Err(CommandError::BoundToOtherHost {
            workspace: workspace.to_owned(),
            session: bound.session().clone(),
            bound: bound.host().clone(),
            asked: asked.clone(),
        }
        .into());
    }
    let same = |asked: &WindowTemplate| bound.window_command().is_some_and(|w| w.same_words(asked));
    if request.window_command.as_ref().is_some_and(|w| !same(w)) {
        return Err(CommandError::OtherWindowCommand(workspace.to_owned()).into());
    }
    Ok(())
}

/// Give window `con_id` the one i3mux mark `mark`: any other i3mux mark
/// it has goes, so the window counts for one shell only, and the windows
/// `take_from` lose `mark`, so the shell has one window only.
fn remark(
    ports: &Ports,
    tree: &Value,
    con_id: u64,
    mark: &Mark,
    take_from: &[u64],
) -> Result<MarkChanges> {
    let marks = window::marks_of(tree, con_id);
    let mut changes = MarkChanges::default();
    changes.removed.extend(
        marks
            .iter()
            .filter(|m| *m != mark)
            .map(|m| (con_id, m.clone())),
    );
    changes
        .removed
        .extend(take_from.iter().map(|id| (*id, mark.clone())));
    if !marks.contains(mark) {
        changes.added.push((con_id, mark.clone()));
    }
    for (id, old) in &changes.removed {
        ports.sway.run(&window::unmark_command(*id, old))?;
    }
    for (id, new) in &changes.added {
        ports.sway.run(&window::mark_command(*id, new))?;
    }
    Ok(changes)
}

/// Check `--here --shell shell`: a live shell of `binding`'s session, not
/// `shown` in another window and not attached elsewhere, unless `force`.
/// It is never made (PLAN-terminal-here finding 3).
fn here_shell(
    workspace: &str,
    binding: &WorkspaceState,
    shells: &[Shell],
    shell: &TerminalName,
    shown: bool,
    force: bool,
) -> Result<()> {
    if shell.session() != binding.session() {
        return Err(CommandError::ShellOfOtherSession {
            shell: shell.clone(),
            session: binding.session().clone(),
            workspace: workspace.to_owned(),
        }
        .into());
    }
    let found =
        shells
            .iter()
            .find(|s| s.name() == shell)
            .ok_or_else(|| CommandError::NoSuchShell {
                terminal: shell.clone(),
                host: binding.host().clone(),
            })?;
    if shown && !force {
        return Err(CommandError::ShownElsewhere(shell.clone()).into());
    }
    if found.is_attached() && !force {
        return Err(CommandError::AlreadyAttached(shell.clone()).into());
    }
    Ok(())
}

/// How a window shows its shell.
#[derive(Debug, Default)]
struct WindowSpec<'a> {
    /// Take the shell over from any other client.
    force: bool,
    /// What a new shell runs instead of the login shell.
    command: Option<&'a str>,
    /// How the wrapper ends the window.
    options: WrapperOptions,
    /// The binding's window command, run instead of the wrapper
    /// (PLAN-terminal-here decision 2).
    template: Option<&'a WindowTemplate>,
    /// The shell runs already. With a window command, i3mux makes a missing
    /// shell first, so that the command can attach it.
    exists: bool,
}

/// Launch the window of `terminal` and mark it; return its container id.
fn open_window(
    ports: &Ports,
    host: &dyn HostExec,
    terminal: &TerminalName,
    spec: &WindowSpec,
) -> Result<u64> {
    let mark = Mark::new(host.host().clone(), terminal.clone());
    let window = window_program(ports, host, terminal, spec)?;
    ports
        .log
        .debug(|| format!("opening {mark}: {}", shell_words::join(window.argv())));
    let title = format!("i3mux:{}:{}", mark.host(), mark.terminal());
    ports
        .launcher
        .spawn(&mark.app_id(), &title, &window, spec.options)?;
    sway_ops::wait_for_window_and_mark(ports.sway, &mark)
}

/// What the window of `terminal` runs: the binding's window command, for
/// which i3mux makes a missing shell first, or else the wrapper's attach.
fn window_program(
    ports: &Ports,
    host: &dyn HostExec,
    terminal: &TerminalName,
    spec: &WindowSpec,
) -> Result<WindowCommand> {
    match spec.template {
        Some(template) => {
            if !spec.exists {
                mux::create(host, terminal, spec.command)?;
            }
            Ok(WindowCommand::Custom(template.argv(terminal)))
        }
        None => window_command(ports, host, terminal, spec.force, spec.command),
    }
}

/// The wrapper's attach of `terminal`. A remote window goes through its
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

/// What `open` did.
#[derive(Debug, PartialEq, Eq)]
enum Opened {
    /// A new window shows the shell, in this workspace.
    Window(String),
    /// The shell already had a window here, in this workspace.
    AlreadyOpen(String),
}

/// Open a window on `terminal`, an existing shell of `host`, in the
/// workspace bound to its session.
///
/// Another program may make the shell (README, "Shells made by other
/// programs"); `open` never creates one. Under the state lock, as
/// [`open_terminal`]: a concurrent `terminal` never reattaches the same
/// shell. The binding is read before the host is asked, so a session that
/// no workspace shows touches no host. A shell that has a window here gets
/// no second one. The window opens where Sway puts new windows, then moves
/// to the bound workspace; focus stays where it was.
fn open(ports: &Ports, host: &Host, terminal: &TerminalName, force: bool) -> Result<Opened> {
    let lock = ports.store.lock()?;
    let workspace = lock
        .state()
        .workspace_of(host, terminal.session())
        .map(str::to_owned)
        .ok_or_else(|| CommandError::NotBound {
            session: terminal.session().clone(),
            host: host.clone(),
        })?;
    let template = lock
        .state()
        .get(&workspace)
        .and_then(WorkspaceState::window_command)
        .cloned();
    if window::session_terminals(&ports.sway.get_tree()?, host, terminal.session())
        .contains(terminal)
    {
        return Ok(Opened::AlreadyOpen(workspace));
    }
    let exec = (ports.hosts)(host);
    let shell = mux::list(exec.as_ref())?
        .into_iter()
        .find(|s| s.name() == terminal)
        .ok_or_else(|| CommandError::NoSuchShell {
            terminal: terminal.clone(),
            host: host.clone(),
        })?;
    if shell.is_attached() && !force {
        return Err(CommandError::AlreadyAttached(terminal.clone()).into());
    }
    let con_id = open_window(
        ports,
        exec.as_ref(),
        terminal,
        &WindowSpec {
            force,
            template: template.as_ref(),
            exists: true,
            ..WindowSpec::default()
        },
    )?;
    ports
        .sway
        .run(&window::move_to_workspace_command(con_id, &workspace))?;
    drop(lock);
    Ok(Opened::Window(workspace))
}

#[derive(Debug)]
struct Detached {
    session: SessionName,
    host: Host,
    terminals: usize,
}

/// Save the layout of `workspace` on the session's host, close its windows,
/// disconnect their shells, and unbind it.
///
/// Plan rule 5: at least one i3mux window is required. Plan rule 7: the
/// layout is written before any window closes, so a failed write leaves
/// every window open.
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
    let saved = SavedSession::new(name.clone(), host.clone(), layout, saved_at_unix)?
        .with_window_command(binding.window_command().cloned());
    let exec = (ports.hosts)(host);
    session::save(exec.as_ref(), &saved)?;
    let closed = sway_ops::kill_all(ports.sway, host, name)?;
    mux::detach(exec.as_ref(), &closed)?;
    lock.state_mut().unbind(workspace);
    lock.commit()?;
    wait_until_disconnected(exec.as_ref(), name, &closed, ports.disconnect_wait)?;
    Ok(Detached {
        session: name.clone(),
        host: host.clone(),
        terminals: saved.layout().terminals().len(),
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

#[derive(Debug)]
struct Attached {
    session: SessionName,
    total: usize,
    /// Shells the host no longer had, started fresh (plan rule 11).
    recreated: usize,
}

/// Restore a saved session of `host` into `workspace`.
///
/// Refuses a shell attached elsewhere unless `force` (plan rule 10), and a
/// workspace that already has i3mux windows (plan rule 6). The workspace is
/// bound before any window opens, so an interrupted attach leaves a bound
/// workspace that `detach` or `kill` can clean up.
fn attach(
    ports: &Ports,
    host: &Host,
    workspace: &str,
    requested: Option<SessionName>,
    force: bool,
) -> Result<AttachOutcome> {
    let exec = (ports.hosts)(host);
    let name = match requested {
        Some(name) => name,
        None => match pick_saved_session(ports, exec.as_ref())? {
            Pick::One(name) => name,
            Pick::Several(candidates) => return Ok(AttachOutcome::Choose(candidates)),
        },
    };
    let saved =
        session::load(exec.as_ref(), &name)?.ok_or_else(|| CommandError::NoSavedSession {
            session: name.clone(),
            host: host.clone(),
        })?;
    let terminals: Vec<TerminalName> = saved.layout().terminals().into_iter().cloned().collect();
    let shells = mux::list(exec.as_ref())?;
    let find = |t: &TerminalName| shells.iter().find(|s| s.name() == t);
    if !force {
        if let Some(busy) = terminals
            .iter()
            .find(|t| find(t).is_some_and(Shell::is_attached))
        {
            return Err(CommandError::AlreadyAttached(busy.clone()).into());
        }
    }
    let recreated = terminals.iter().filter(|t| find(t).is_none()).count();

    let mut lock = ports.store.lock()?;
    let tree = ports.sway.get_tree()?;
    if window::find_workspace(&tree, workspace)
        .is_some_and(|ws| !window::i3mux_windows_in(ws).is_empty())
    {
        return Err(CommandError::WorkspaceHasTerminals(workspace.to_owned()).into());
    }
    let template = saved.window_command();
    lock.state_mut().bind(
        workspace,
        WorkspaceState::new(host.clone(), name.clone()).with_window_command(template.cloned()),
    )?;
    lock.commit()?;
    for step in layout::restore_plan(saved.layout()) {
        match step {
            Step::Launch(terminal) => {
                let spec = WindowSpec {
                    force,
                    template,
                    exists: find(&terminal).is_some(),
                    ..WindowSpec::default()
                };
                open_window(ports, exec.as_ref(), &terminal, &spec)?;
            }
            Step::Sway(command) => ports.sway.run(command)?,
        }
    }
    Ok(AttachOutcome::Attached(Attached {
        session: name,
        total: terminals.len(),
        recreated,
    }))
}

enum Pick {
    One(SessionName),
    Several(Vec<SessionName>),
}

/// What `attach --rebind-only` did.
#[derive(Debug, PartialEq, Eq)]
struct Rebound {
    workspace: String,
    /// The host the binding left; `None` if it was on the new host already.
    from: Option<Host>,
    /// The windows that now carry the new host in their mark.
    remarked: usize,
}

/// Move the binding of `session` to `host`, and re-mark the session's
/// windows that show another host (PLAN-terminal-here decision 3).
///
/// It contacts no host, starts no master, and opens, closes or attaches
/// no window: the windows keep running what they run. Under the state lock
/// throughout. The binding is committed first, then the windows are
/// re-marked; a run cut in between leaves windows with the old host, and
/// a second run re-marks them. A second run changes nothing else.
fn rebind(ports: &Ports, host: &Host, session: &SessionName) -> Result<Rebound> {
    let mut lock = ports.store.lock()?;
    let bound: Vec<(String, Host)> = lock
        .state()
        .bindings()
        .filter(|(_, b)| b.session() == session)
        .map(|(ws, b)| (ws.to_owned(), b.host().clone()))
        .collect();
    let (workspace, old) = match bound.as_slice() {
        [] => return Err(CommandError::NothingToRebind(session.clone()).into()),
        [(workspace, old)] => (workspace.clone(), old.clone()),
        several => {
            let workspaces: Vec<&str> = several.iter().map(|(ws, _)| ws.as_str()).collect();
            return Err(CommandError::BoundTwice {
                session: session.clone(),
                workspaces: workspaces.join(" and "),
            }
            .into());
        }
    };
    if &old != host {
        lock.state_mut().rebind(&workspace, host)?;
        lock.commit()?;
    }
    let stale: Vec<(u64, Mark)> = window::i3mux_windows_in(&ports.sway.get_tree()?)
        .into_iter()
        .filter(|(_, mark)| mark.terminal().session() == session && mark.host() != host)
        .collect();
    for (con_id, mark) in &stale {
        let new = Mark::new(host.clone(), mark.terminal().clone());
        ports.sway.run(&window::unmark_command(*con_id, mark))?;
        ports.sway.run(&window::mark_command(*con_id, &new))?;
    }
    Ok(Rebound {
        workspace,
        from: (&old != host).then_some(old),
        remarked: stale.len(),
    })
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
/// close the windows (before the shells, so no wrapper shows an error),
/// kill the shells, delete the saved layout, remove the binding. An
/// unreachable host therefore leaves every window open.
fn kill(ports: &Ports, host: &Host, name: &SessionName) -> Result<Killed> {
    let exec = (ports.hosts)(host);
    let mut lock = ports.store.lock()?;
    mux::list(exec.as_ref())?;
    let windows = sway_ops::kill_all(ports.sway, host, name)?.len();
    let shells = mux::kill_session(exec.as_ref(), name)?.len();
    let layout_deleted = session::delete(exec.as_ref(), name)?;
    let unbound = lock.state_mut().unbind_session(host, name).is_some();
    if windows == 0 && shells == 0 && !layout_deleted && !unbound {
        return Err(CommandError::NoSuchSession {
            session: name.clone(),
            host: host.clone(),
        }
        .into());
    }
    lock.commit()?;
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
    /// The binding's window command, if it has one.
    window_command: Option<WindowTemplate>,
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
        window_command: binding.window_command().cloned(),
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

/// The row, then `  connection: ...` for a remote host and `  window
/// command: ...` for a binding with one. The first line is a `sessions`
/// row; scripts/i3mux-rofi reads its first word and host.
fn format_status(report: &StatusReport) -> Vec<String> {
    let mut lines = format_rows(std::slice::from_ref(&report.row));
    if let Some(connection) = &report.connection {
        lines.push(format!("  connection: {connection}"));
    }
    if let Some(window_command) = &report.window_command {
        lines.push(format!("  window command: {}", window_command.as_str()));
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
