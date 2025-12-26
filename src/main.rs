mod connection;
mod layout;
mod session;
mod types;
mod window;
mod wm;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

// Global verbose flag
static VERBOSE: AtomicBool = AtomicBool::new(false);

// Debug logging macro - only logs when verbose flag is set
macro_rules! debug {
    ($($arg:tt)*) => {
        if VERBOSE.load(Ordering::Relaxed) {
            eprintln!("[i3mux] {}", format!($($arg)*));
        }
    };
}

use connection::create_connection;
use layout::Layout;
use session::RemoteSession;
use types::{RemoteHost, SessionName};
use window::{I3muxWindow, wait_for_window_and_mark};
use wm::{WmBackend, WmType};

const MARKER: &str = "i3mux:"; // Marker prefix for window titles (for initial window matching)
const LOCAL_DISPLAY: &str = "\x1b[3mlocal\x1b[0m"; // Italicized "local"

// Remote helper script - uploaded to remote hosts for reliable command execution
const REMOTE_HELPER_SCRIPT: &str = include_str!("remote-helper.sh");
const REMOTE_HELPER_PATH: &str = "/tmp/i3mux-helper.sh";

// Wrapper script - runs locally to launch terminals with proper setup
const WRAPPER_SCRIPT: &str = include_str!("wrapper.sh");
const WRAPPER_PATH: &str = "/tmp/i3mux-wrapper.sh";

#[derive(Parser)]
#[command(name = "i3mux")]
#[command(about = "Persistent terminal sessions with i3 workspace integration")]
#[command(version)]
struct Cli {
    /// Remote host (e.g., 'deepthought' or 'user@host')
    #[arg(short, long)]
    remote: Option<String>,

    /// Session name (optional, required if multiple sessions exist)
    #[arg(short, long)]
    session: Option<String>,

    /// Enable verbose debug logging
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Activate i3mux for current workspace
    Activate {
        /// Remote host (for remote sessions)
        #[arg(short, long)]
        remote: Option<String>,

        /// Session name (optional)
        #[arg(short, long)]
        session: Option<String>,
    },

    /// Detach current workspace and save session to remote
    Detach {
        /// Session name to save as
        #[arg(short, long)]
        session: Option<String>,
    },

    /// Attach to a saved session
    Attach {
        /// Remote host
        #[arg(short, long)]
        remote: Option<String>,

        /// Session name
        #[arg(short, long)]
        session: Option<String>,

        /// Force attach (break existing lock)
        #[arg(long)]
        force: bool,
    },

    /// List available sessions on remote
    Sessions {
        /// Remote host
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// Kill a saved session
    Kill {
        /// Remote host
        #[arg(short, long)]
        remote: Option<String>,

        /// Session name
        #[arg(short, long)]
        session: String,
    },

    /// Launch terminal (called by i3 keybind)
    Terminal,

    /// Clean up workspace state if no sessions remain (internal command)
    #[command(hide = true)]
    CleanupWorkspace {
        /// Workspace name (e.g., "4" for workspace 4)
        workspace: String,
    },
}

/// Local ephemeral state (current workspace activations)
#[derive(Debug, Serialize, Deserialize, Default)]
struct LocalState {
    /// Active workspace sessions
    workspaces: HashMap<String, WorkspaceState>,

    /// Lock holder processes (kept alive to maintain server-side locks)
    #[serde(skip)]
    lock_holders: HashMap<String, std::process::Child>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct WorkspaceState {
    session_type: String, // "local" or "remote"
    host: String,
    session_name: Option<String>,
    next_socket_id: u32,
    sockets: HashMap<String, SocketInfo>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SocketInfo {
    socket_id: String,
}

impl LocalState {
    fn path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .context("Could not find config directory")?
            .join("i3mux");
        fs::create_dir_all(&config_dir)?;
        Ok(config_dir.join("state.json"))
    }

    fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(LocalState::default());
        }
        let contents = fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&contents)?)
    }

    fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let contents = serde_json::to_string_pretty(self)?;
        fs::write(&path, contents)?;
        Ok(())
    }
}

impl Drop for LocalState {
    fn drop(&mut self) {
        // Clean up any remaining lock holder processes
        for (lock_key, mut lock_process) in self.lock_holders.drain() {
            eprintln!("Cleaning up lock holder for {}", lock_key);
            let _ = lock_process.kill();
            let _ = lock_process.wait();
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Set global verbose flag
    VERBOSE.store(cli.verbose, Ordering::Relaxed);

    match cli.command {
        None => {
            // Default: activate current workspace
            activate(cli.remote, cli.session)
        }
        Some(Commands::Activate { remote, session }) => {
            activate(remote.or(cli.remote), session.or(cli.session))
        }
        Some(Commands::Detach { session }) => detach(session),
        Some(Commands::Attach {
            remote,
            session,
            force,
        }) => attach(remote.or(cli.remote), session.or(cli.session), force),
        Some(Commands::Sessions { remote }) => list_sessions(remote.or(cli.remote)),
        Some(Commands::Kill { remote, session }) => kill_session(remote.or(cli.remote), session),
        Some(Commands::Terminal) => terminal(),
        Some(Commands::CleanupWorkspace { workspace }) => cleanup_workspace(&workspace),
    }
}

/// Check if abduco is available locally
fn check_abduco_local() -> Result<()> {
    match Command::new("which").arg("abduco").output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => anyhow::bail!(
            "abduco not found. Please install it:\n\
            - Arch Linux: sudo pacman -S abduco\n\
            - Debian/Ubuntu: sudo apt install abduco\n\
            - macOS: brew install abduco\n\
            - Or build from source: https://github.com/martanne/abduco"
        ),
    }
}

/// Check if abduco is available on remote host using helper script
fn check_abduco_remote(remote_host: &str) -> Result<()> {
    // Ensure helper script is uploaded
    ensure_remote_helper(remote_host)?;

    // Use helper script to check dependencies
    let output = Command::new("ssh")
        .arg(remote_host)
        .arg(format!("bash -lc '{} check-deps'", REMOTE_HELPER_PATH))
        .output()
        .context("Failed to check for abduco on remote host")?;

    if !output.status.success() {
        let error_msg = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{}", error_msg.trim());
    }

    debug!("abduco found at: {}", String::from_utf8_lossy(&output.stdout).trim());
    Ok(())
}

/// Ensure the wrapper script exists locally
fn ensure_wrapper_script() -> Result<()> {
    use std::io::Write;

    let path = std::path::Path::new(WRAPPER_PATH);

    // Always write the script (it's cheap and ensures we have latest version)
    let mut file = std::fs::File::create(path)
        .context("Failed to create wrapper script")?;
    file.write_all(WRAPPER_SCRIPT.as_bytes())
        .context("Failed to write wrapper script")?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }

    Ok(())
}

/// Ensure the helper script is uploaded and executable on a remote host
fn ensure_remote_helper(remote_host: &str) -> Result<()> {
    debug!("Ensuring helper script is present on {}", remote_host);

    // Check if script exists and has correct version
    let version_check = Command::new("ssh")
        .arg(remote_host)
        .arg(format!("{} version 2>/dev/null || echo ''", REMOTE_HELPER_PATH))
        .output()
        .context("Failed to check remote helper version")?;

    let remote_version = String::from_utf8_lossy(&version_check.stdout).trim().to_string();

    // Extract version from script (look for VERSION="x.x.x")
    let local_version = REMOTE_HELPER_SCRIPT
        .lines()
        .find(|line| line.contains("VERSION="))
        .and_then(|line| line.split('"').nth(1))
        .unwrap_or("unknown");

    if remote_version == local_version {
        debug!("Remote helper already at version {}", local_version);
        return Ok(());
    }

    debug!("Uploading helper script to remote (version {})", local_version);

    // Upload script via stdin
    let mut upload = Command::new("ssh")
        .arg(remote_host)
        .arg(format!("cat > {}", REMOTE_HELPER_PATH))
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("Failed to start SSH upload")?;

    if let Some(mut stdin) = upload.stdin.take() {
        use std::io::Write;
        stdin.write_all(REMOTE_HELPER_SCRIPT.as_bytes())
            .context("Failed to write helper script")?;
    }

    let status = upload.wait().context("Failed to wait for upload")?;
    if !status.success() {
        anyhow::bail!("Failed to upload helper script to {}", remote_host);
    }

    // Make script executable
    let chmod = Command::new("ssh")
        .arg(remote_host)
        .arg(format!("chmod +x {}", REMOTE_HELPER_PATH))
        .status()
        .context("Failed to make helper script executable")?;

    if !chmod.success() {
        anyhow::bail!("Failed to make helper script executable on {}", remote_host);
    }

    debug!("Helper script uploaded to remote successfully");
    Ok(())
}

/// Activate i3mux for current workspace
fn activate(remote: Option<String>, session_name: Option<String>) -> Result<()> {
    let backend = WmBackend::connect()?;
    let (ws_name, ws_num) = get_focused_workspace(&backend)?;

    let mut state = LocalState::load()?;

    // Validate inputs at CLI boundary
    let remote_host = remote.map(|r| RemoteHost::new(r)).transpose()?;

    let validated_session_name = session_name.map(|name| SessionName::new(name)).transpose()?;

    // Check abduco availability
    match &remote_host {
        None => check_abduco_local()?,
        Some(host) => check_abduco_remote(host.as_str())?,
    }

    // Ensure SSH control socket directory exists
    if remote_host.is_some() {
        std::fs::create_dir_all("/tmp/i3mux/sockets")?;
    }

    let (session_type, host_str) = match &remote_host {
        None => ("local", None),
        Some(h) => ("remote", Some(h.as_str().to_string())),
    };

    state.workspaces.insert(
        ws_name.clone(),
        WorkspaceState {
            session_type: session_type.to_string(),
            host: host_str.clone().unwrap_or_else(|| "local".to_string()),
            session_name: validated_session_name.map(|n| n.as_str().to_string()),
            next_socket_id: 1,
            sockets: HashMap::new(),
        },
    );

    state.save()?;

    println!("✓ Workspace {} activated", ws_num);
    if let Some(host) = &host_str {
        println!("  Remote: {}", host);
    }

    // Launch first terminal
    terminal()?;

    Ok(())
}

/// Detach current workspace and save session
fn detach(session_name: Option<String>) -> Result<()> {
    let backend = WmBackend::connect()?;
    let (ws_name, ws_num) = get_focused_workspace(&backend)?;

    let mut state = LocalState::load()?;

    let ws_state = state
        .workspaces
        .get(&ws_name)
        .context("Workspace not i3mux-bound")?
        .clone();

    if ws_state.session_type == "local" {
        anyhow::bail!("Cannot detach local sessions (use remote sessions for detach/attach)");
    }

    // Capture layout using marks (most reliable identification method)
    let layout = Layout::capture_from_workspace_num(ws_num, &backend)?
        .context("No i3mux terminals found in workspace")?;

    // Determine session name and validate at boundary
    let final_session_name_str = session_name
        .or(ws_state.session_name)
        .unwrap_or_else(|| format!("ws{}", ws_num));
    let final_session_name = SessionName::new(final_session_name_str)?;

    // Parse remote host (if "local", use None)
    let remote_host = if ws_state.host == "local" {
        None
    } else {
        Some(RemoteHost::new(ws_state.host.clone())?)
    };

    // Create remote session (internal code uses validated inputs)
    let remote_session = RemoteSession::new(
        final_session_name.as_str().to_string(),
        ws_name.clone(),
        ws_state.host.clone(),
        layout,
    )?;

    // Save to remote
    let host_conn = create_connection(remote_host.as_ref().map(|h| h.as_str()))?;
    remote_session.save_to_remote(host_conn.as_ref())?;

    println!("✓ Session '{}' saved to {}", final_session_name, ws_state.host);
    println!("  Layout captured: {} terminals", remote_session.layout.get_sockets().len());

    // Close all i3mux terminals (identified by marks)
    window::kill_i3mux_windows_in_workspace(&backend, ws_num)?;

    // Clean up lock holder process and release lock
    let lock_key = format!("{}:{}", ws_state.host, final_session_name.as_str());
    if let Some(mut lock_process) = state.lock_holders.remove(&lock_key) {
        // Kill the lock holder process (this will cause remote lock cleanup via EXIT trap)
        let _ = lock_process.kill();
        let _ = lock_process.wait();
    }

    // Explicitly release lock on remote
    let _ = host_conn.release_lock(final_session_name.as_str());

    // Remove from local state
    state.workspaces.remove(&ws_name);
    state.save()?;

    println!("✓ Workspace {} detached", ws_num);

    Ok(())
}

/// Attach to a saved session
fn attach(
    remote: Option<String>,
    session_name: Option<String>,
    force: bool,
) -> Result<()> {
    // Validate remote host at CLI boundary
    let remote_host = remote.map(|r| RemoteHost::new(r)).transpose()?;

    // Check abduco availability
    match &remote_host {
        None => check_abduco_local()?,
        Some(host) => check_abduco_remote(host.as_str())?,
    }

    // Ensure SSH control socket directory exists
    if remote_host.is_some() {
        std::fs::create_dir_all("/tmp/i3mux/sockets")?;
    }

    // Create connection (None = local, Some = remote)
    let host_conn = create_connection(remote_host.as_ref().map(|h| h.as_str()))?;

    // List available sessions
    let sessions = RemoteSession::list_remote_sessions(host_conn.as_ref())?;

    let host_display = remote_host.as_ref()
        .map(|h| h.as_str().to_string())
        .unwrap_or_else(|| LOCAL_DISPLAY.to_string());

    if sessions.is_empty() {
        anyhow::bail!("No sessions found on {}", host_display);
    }

    // Determine which session to attach
    let final_session_name_str = if let Some(name) = session_name {
        if !sessions.contains(&name) {
            anyhow::bail!("Session '{}' not found on {}", name, host_display);
        }
        name
    } else if sessions.len() == 1 {
        sessions[0].clone()
    } else {
        // Multiple sessions, return exit code 2 for rofi integration
        eprintln!("Multiple sessions available:");
        for s in &sessions {
            eprintln!("  - {}", s);
        }
        eprintln!("\nSpecify session with -s/--session");
        std::process::exit(2);
    };

    // Validate session name at CLI boundary
    let final_session_name = SessionName::new(final_session_name_str)?;

    // Load session
    let mut session = RemoteSession::load_from_remote(host_conn.as_ref(), final_session_name.as_str())?;

    // Acquire lock
    let (lock, lock_holder) = host_conn.acquire_lock(final_session_name.as_str(), force)?;
    session.lock = Some(lock.clone());
    session.save_to_remote(host_conn.as_ref())?;

    println!("✓ Lock acquired for session '{}'", final_session_name);

    // Check workspace doesn't have existing i3mux terminals (non-i3mux windows are fine)
    let backend = WmBackend::connect()?;
    let (ws_name, ws_num) = get_focused_workspace(&backend)?;

    if window::workspace_has_i3mux_windows(ws_num, &backend)? {
        anyhow::bail!("Workspace {} already has i3mux terminals. Detach or clear them first.", ws_num);
    }

    // Restore layout and launch terminals
    restore_layout(&backend, &session, &ws_name, &host_display)?;

    // Update local state
    let mut state = LocalState::load()?;
    let (session_type, host_str) = match &remote_host {
        None => ("local", "local".to_string()),
        Some(h) => ("remote", h.as_str().to_string()),
    };

    state.workspaces.insert(
        ws_name.clone(),
        WorkspaceState {
            session_type: session_type.to_string(),
            host: host_str.clone(),
            session_name: Some(final_session_name.as_str().to_string()),
            next_socket_id: session.layout.get_sockets().len() as u32 + 1,
            sockets: session
                .layout
                .get_sockets()
                .into_iter()
                .map(|s| (s.clone(), SocketInfo { socket_id: s }))
                .collect(),
        },
    );

    // Store lock holder process if present
    if let Some(lock_process) = lock_holder {
        let lock_key = format!("{}:{}", host_str, final_session_name.as_str());
        state.lock_holders.insert(lock_key, lock_process);
    }

    state.save()?;

    println!("✓ Attached to session '{}' in workspace {}", final_session_name, ws_num);

    Ok(())
}

/// List sessions on remote
fn list_sessions(remote: Option<String>) -> Result<()> {
    // Validate remote host at CLI boundary
    let remote_host = remote.map(|r| RemoteHost::new(r)).transpose()?;
    let host_display = remote_host.as_ref()
        .map(|h| h.as_str().to_string())
        .unwrap_or_else(|| LOCAL_DISPLAY.to_string());

    let host_conn = create_connection(remote_host.as_ref().map(|h| h.as_str()))?;
    let sessions = RemoteSession::list_remote_sessions(host_conn.as_ref())?;

    if sessions.is_empty() {
        println!("No sessions on {}", host_display);
        return Ok(());
    }

    println!("Sessions on {}:\n", host_display);
    for name in &sessions {
        let session = RemoteSession::load_from_remote(host_conn.as_ref(), name)?;
        let locked = if let Some(lock) = &session.lock {
            if host_conn.is_lock_valid(&lock)? {
                format!(" [LOCKED by {}]", lock.locked_by)
            } else {
                " [stale lock]".to_string()
            }
        } else {
            "".to_string()
        };

        println!("  {} - {} terminals{}", name, session.layout.get_sockets().len(), locked);
    }

    Ok(())
}

/// Kill a saved session
fn kill_session(remote: Option<String>, session: String) -> Result<()> {
    // Validate inputs at CLI boundary
    let remote_host = remote.map(|r| RemoteHost::new(r)).transpose()?;
    let session_name = SessionName::new(session)?;
    let host_display = remote_host.as_ref()
        .map(|h| h.as_str().to_string())
        .unwrap_or_else(|| LOCAL_DISPLAY.to_string());

    // Create connection and delete session (None = local, Some = remote)
    let host_conn = create_connection(remote_host.as_ref().map(|h| h.as_str()))?;
    host_conn.delete_session(session_name.as_str())?;

    println!("✓ Session '{}' deleted from {}", session_name, host_display);
    Ok(())
}

/// Launch terminal (smart detection)
fn terminal() -> Result<()> {
    let backend = WmBackend::connect()?;
    let (ws_name, _) = get_focused_workspace(&backend)?;

    let state = LocalState::load()?;

    // Check if workspace is i3mux-bound
    if state.workspaces.get(&ws_name).is_none() {
        return launch_normal_terminal(backend.wm_type());
    }

    // Workspace is i3mux-bound - always launch i3mux terminal
    // (The old logic checked focused window type, but that doesn't make sense:
    //  if the workspace is bound to i3mux, ALL terminals should be i3mux terminals)
    launch_i3mux_terminal(&ws_name, backend.wm_type())?;

    Ok(())
}

// Helper functions

fn get_focused_workspace(backend: &WmBackend) -> Result<(String, i32)> {
    let workspaces = backend.get_workspaces()?;
    for ws in workspaces {
        if ws.focused {
            return Ok((ws.num.to_string(), ws.num));
        }
    }
    anyhow::bail!("No focused workspace found")
}

/// Build terminal-specific arguments to set window instance/app_id
///
/// Different terminals have different CLI options for setting the window identifier.
/// On X11 (i3), this sets the WM_CLASS instance. On Wayland (Sway), this sets the app_id.
fn build_terminal_instance_args(terminal: &str, instance: &str, wm_type: WmType) -> Vec<String> {
    // Extract just the binary name from the path
    let terminal_name = std::path::Path::new(terminal)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(terminal);

    match terminal_name {
        // Wayland-native terminals
        "foot" => vec!["--app-id".to_string(), instance.to_string()],

        // Terminals that work on both X11 and Wayland
        "alacritty" => match wm_type {
            WmType::Sway => vec!["--class".to_string(), instance.to_string()],
            WmType::I3 => vec!["--class".to_string(), format!("Alacritty,{}", instance)],
        },
        "kitty" => vec!["--class".to_string(), instance.to_string()],

        // X11-only terminals
        "xterm" => vec!["-name".to_string(), instance.to_string()],
        "urxvt" | "rxvt-unicode" => vec!["-name".to_string(), instance.to_string()],
        "st" => vec!["-n".to_string(), instance.to_string()],

        // Default based on WM type
        _ => match wm_type {
            WmType::Sway => vec!["--app-id".to_string(), instance.to_string()],
            WmType::I3 => vec!["-name".to_string(), instance.to_string()],
        },
    }
}

fn get_terminal_command(wm_type: WmType) -> String {
    std::env::var("TERMINAL").unwrap_or_else(|_| match wm_type {
        WmType::Sway => "foot".to_string(),
        WmType::I3 => "i3-sensible-terminal".to_string(),
    })
}

fn get_user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string())
}

fn launch_normal_terminal(wm_type: WmType) -> Result<()> {
    Command::new(get_terminal_command(wm_type))
        .spawn()
        .context("Failed to launch terminal")?;
    Ok(())
}

fn launch_i3mux_terminal(ws_name: &str, wm_type: WmType) -> Result<()> {
    debug!("launch_i3mux_terminal called for workspace: {}", ws_name);

    // Ensure wrapper script exists
    ensure_wrapper_script()?;

    let mut state = LocalState::load()?;

    let socket = {
        let ws_state = state
            .workspaces
            .get_mut(ws_name)
            .context("Workspace not i3mux-bound")?;

        let socket = format!("ws{}-{:03}", ws_name, ws_state.next_socket_id);
        debug!("Generated socket ID: {}", socket);
        ws_state.next_socket_id += 1;
        ws_state.sockets.insert(socket.clone(), SocketInfo { socket_id: socket.clone() });
        socket
    };

    let (title, attach_cmd, cleanup_cmd) = {
        let ws_state = state
            .workspaces
            .get(ws_name)
            .context("Workspace not i3mux-bound")?;

        let title = if ws_state.session_type == "local" {
            format!("{}local:{}", MARKER, socket)
        } else {
            format!("{}{}:{}", MARKER, ws_state.host, socket)
        };

        // Escape the title for use in PROMPT_COMMAND (needs extra escaping for SSH)
        let title_for_prompt = title.replace("\\", "\\\\").replace("\"", "\\\"").replace("$", "\\$");

        let user_shell = get_user_shell();
        debug!("Using user shell: {}", user_shell);

        let attach_cmd = if ws_state.session_type == "local" {
            // Local: Direct abduco attach
            let prompt_cmd_val = format!("echo -ne \\\"\\\\033]0;{}\\\\007\\\"", title_for_prompt);
            format!(
                r#"bash -c "export PROMPT_COMMAND='{}'; exec abduco -A /tmp/{} {}""#,
                prompt_cmd_val, socket, user_shell
            )
        } else {
            // Remote: Use helper script to attach (ensures PATH is set correctly)
            format!(
                r#"TERM=xterm-256color ssh -o ControlPath=/tmp/i3mux/sockets/%r@%h:%p -o ControlMaster=auto -o ControlPersist=10m -tt {} 'bash -l -c "exec {} attach {}"'"#,
                ws_state.host, REMOTE_HELPER_PATH, socket
            )
        };

        // Cleanup script to run after terminal exits
        // This cleans up remote session files AND local workspace state
        let cleanup_cmd = {
            let ws_prefix = format!("ws{}", ws_name);
            let session_cleanup = if let Some(session_name) = &ws_state.session_name {
                if ws_state.session_type == "local" {
                    // Local cleanup: Remove session files if no sockets remain
                    format!(
                        r#"if ! ls /tmp/{ws_prefix}-* &>/dev/null; then rm -f /tmp/i3mux/sessions/{session}.json /tmp/i3mux/locks/{session}.lock; fi"#,
                        ws_prefix = ws_prefix,
                        session = session_name
                    )
                } else {
                    // Remote cleanup: Use helper script to check and clean up remote session files
                    format!(
                        r#"ssh -o ControlPath=/tmp/i3mux/sockets/%r@%h:%p {host} 'bash -lc "{helper} cleanup-check {ws_prefix} {session}"' 2>/dev/null || true"#,
                        host = ws_state.host,
                        helper = REMOTE_HELPER_PATH,
                        ws_prefix = ws_prefix,
                        session = session_name
                    )
                }
            } else {
                String::new()
            };

            // Always clean up workspace state if no sockets remain (for both local and remote)
            // Get the binary path for i3mux
            let i3mux_bin = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
                .unwrap_or_else(|| "i3mux".to_string());

            format!(
                r#"{session_cleanup}; {bin} cleanup-workspace {ws} 2>/dev/null || true"#,
                session_cleanup = session_cleanup,
                bin = i3mux_bin,
                ws = ws_name
            )
        };

        (title, attach_cmd, cleanup_cmd)
    };

    state.save()?;

    let ws_state = state.workspaces.get(ws_name).unwrap();

    debug!("Session type: {}", ws_state.session_type);
    debug!("Host: {}", ws_state.host);
    debug!("Title: {}", title);
    debug!("Attach command: {}", attach_cmd);

    // Build wrapper script invocation
    // Pass PROMPT_COMMAND for remote sessions to maintain title
    let prompt_cmd = if ws_state.session_type == "remote" {
        format!("echo -ne \"\\033]0;{}\\007\"", title.replace("\\", "\\\\").replace("\"", "\\\"").replace("$", "\\$"))
    } else {
        String::new()
    };

    let wrapper_args = vec![
        socket.as_str(),
        &title,
        &attach_cmd,
        &cleanup_cmd,
        &prompt_cmd,
    ];

    debug!("Wrapper script: {} with args: {:?}", WRAPPER_PATH, wrapper_args);
    debug!("Terminal command: {}", get_terminal_command(wm_type));

    // Get the host for creating the I3muxWindow identity
    let host = ws_state.host.clone();

    // Generate instance name (same format as marks)
    let instance = I3muxWindow::mark_from_parts(&host, &socket);

    // Build terminal command with instance-specific args
    let terminal = get_terminal_command(wm_type);
    let instance_args = build_terminal_instance_args(&terminal, &instance, wm_type);

    debug!("Instance name: {}", instance);
    debug!("Terminal args: {:?}", instance_args);

    // Spawn the terminal with instance set via terminal-specific CLI args
    let mut cmd = Command::new(&terminal);
    cmd.args(&instance_args)
        .arg("-T")
        .arg(&title)
        .arg("-e")
        .arg(WRAPPER_PATH)
        .args(&wrapper_args);

    cmd.spawn().context("Failed to launch i3mux terminal")?;

    // Wait for window to appear and apply i3mux mark
    let backend = WmBackend::connect()?;
    wait_for_window_and_mark(&backend, &instance, &host, &socket)?;

    debug!("launch_i3mux_terminal completed successfully");
    Ok(())
}

/// Clean up workspace state if no active sessions remain
fn cleanup_workspace(ws_name: &str) -> Result<()> {
    debug!("cleanup_workspace called for workspace: {}", ws_name);

    let mut state = LocalState::load()?;

    // Check if workspace exists in state
    if !state.workspaces.contains_key(ws_name) {
        debug!("Workspace {} not in state, nothing to clean up", ws_name);
        return Ok(());
    }

    // Check if any socket files exist for this workspace
    let ws_prefix = format!("ws{}", ws_name);
    let socket_pattern = format!("/tmp/{}-*", ws_prefix);

    debug!("Checking for socket files: {}", socket_pattern);

    // Use glob to check for socket files
    let has_sockets = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("ls {} 2>/dev/null", socket_pattern))
        .output()?
        .status
        .success();

    if has_sockets {
        debug!("Socket files still exist, not cleaning up workspace state");
        return Ok(());
    }

    // No sockets remain, remove workspace state
    debug!("No socket files found, removing workspace state for {}", ws_name);
    state.workspaces.remove(ws_name);
    state.save()?;

    debug!("Workspace {} state cleaned up successfully", ws_name);
    Ok(())
}

fn restore_layout(
    backend: &WmBackend,
    session: &RemoteSession,
    _ws_name: &str,
    remote_host: &str,
) -> Result<()> {
    // Generate i3 commands to recreate layout
    let commands = session.layout.generate_i3_commands(0);

    // Get sockets to restore
    let sockets = session.layout.get_sockets();

    println!("Restoring layout with {} terminals...", sockets.len());

    // Launch terminals in order, executing layout commands between them
    for (i, socket_id) in sockets.iter().enumerate() {
        // Launch terminal for this socket
        let title = format!("{}{}:{}", MARKER, remote_host, socket_id);

        // Generate instance name (same format as marks)
        let instance = I3muxWindow::mark_from_parts(remote_host, socket_id);

        let attach_cmd = format!(
            r#"TERM=xterm-256color ssh -o ControlPath=/tmp/i3mux/sockets/%r@%h:%p -o ControlMaster=auto -o ControlPersist=10m -t {} 'exec bash -lc "{} attach {}"'"#,
            remote_host, REMOTE_HELPER_PATH, socket_id
        );

        let wrapper = format!(
            r#"echo -ne '\033]0;{}\007'; {}; echo 'Session ended.'"#,
            title, attach_cmd
        );

        // Build terminal command with instance-specific args
        let terminal = get_terminal_command(backend.wm_type());
        let instance_args = build_terminal_instance_args(&terminal, &instance, backend.wm_type());

        // Spawn terminal with instance set via terminal-specific CLI args
        let mut cmd = Command::new(&terminal);
        cmd.args(&instance_args)
            .arg("-T")
            .arg(&title)
            .arg("-e")
            .arg("bash")
            .arg("-c")
            .arg(&wrapper);

        cmd.spawn().context("Failed to spawn terminal for layout restore")?;

        // Wait for window to appear and apply i3mux mark
        wait_for_window_and_mark(backend, &instance, remote_host, socket_id)?;

        // Execute layout command if available
        if i < commands.len() {
            backend.run_command(&commands[i])?;
        }
    }

    Ok(())
}
