mod connection;
mod layout;
mod session;
mod types;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use i3ipc::I3Connection;
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

const MARKER: &str = "i3mux:"; // Visible marker prefix for i3mux-managed terminals
const LOCAL_DISPLAY: &str = "\x1b[3mlocal\x1b[0m"; // Italicized "local"

// Remote helper script - uploaded to remote hosts for reliable command execution
const REMOTE_HELPER_SCRIPT: &str = include_str!("remote-helper.sh");
const REMOTE_HELPER_PATH: &str = "/tmp/i3mux-helper.sh";

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

/// Ensure the remote helper script is uploaded and executable on the remote host
fn ensure_remote_helper(remote_host: &str) -> Result<()> {
    debug!("Ensuring remote helper script is present on {}", remote_host);

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

    debug!("Uploading remote helper script (version {})", local_version);

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

    debug!("Remote helper script uploaded successfully");
    Ok(())
}

/// Activate i3mux for current workspace
fn activate(remote: Option<String>, session_name: Option<String>) -> Result<()> {
    let mut conn = I3Connection::connect()?;
    let (ws_name, ws_num) = get_focused_workspace(&mut conn)?;

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
    let mut conn = I3Connection::connect()?;
    let (ws_name, ws_num) = get_focused_workspace(&mut conn)?;

    let mut state = LocalState::load()?;

    let ws_state = state
        .workspaces
        .get(&ws_name)
        .context("Workspace not i3mux-bound")?
        .clone();

    if ws_state.session_type == "local" {
        anyhow::bail!("Cannot detach local sessions (use remote sessions for detach/attach)");
    }

    // Capture layout
    let tree = conn.get_tree()?;
    let workspace_node = find_workspace(&tree, ws_num)
        .context("Could not find workspace in i3 tree")?;

    let layout = Layout::capture_from_workspace(workspace_node)?
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

    // Close all i3mux terminals
    // We need to kill windows individually by matching their titles,
    // because i3's criteria matching doesn't handle the zero-width space character reliably
    let tree = conn.get_tree()?;
    if let Some(ws_node) = find_workspace(&tree, ws_num) {
        kill_i3mux_windows(&mut conn, ws_node)?;
    }

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

    // Check abduco availability (needed to restore sessions)
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
    let mut conn = I3Connection::connect()?;
    let (ws_name, ws_num) = get_focused_workspace(&mut conn)?;

    let tree = conn.get_tree()?;
    if let Some(ws_node) = find_workspace(&tree, ws_num) {
        if has_i3mux_windows(ws_node) {
            anyhow::bail!("Workspace {} already has i3mux terminals. Detach or clear them first.", ws_num);
        }
    }

    // Restore layout and launch terminals
    restore_layout(&mut conn, &session, &ws_name, &host_display)?;

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
    let mut conn = I3Connection::connect()?;
    let (ws_name, _) = get_focused_workspace(&mut conn)?;

    let state = LocalState::load()?;

    // Check if workspace is i3mux-bound
    if state.workspaces.get(&ws_name).is_none() {
        return launch_normal_terminal();
    }

    // Workspace is i3mux-bound - always launch i3mux terminal
    // (The old logic checked focused window type, but that doesn't make sense:
    //  if the workspace is bound to i3mux, ALL terminals should be i3mux terminals)
    launch_i3mux_terminal(&ws_name)?;

    Ok(())
}

// Helper functions

fn get_focused_workspace(conn: &mut I3Connection) -> Result<(String, i32)> {
    let workspaces = conn.get_workspaces()?;
    for ws in workspaces.workspaces {
        if ws.focused {
            return Ok((ws.num.to_string(), ws.num));
        }
    }
    anyhow::bail!("No focused workspace found")
}

fn find_workspace<'a>(node: &'a i3ipc::reply::Node, ws_num: i32) -> Option<&'a i3ipc::reply::Node> {
    use i3ipc::reply::NodeType;
    if node.nodetype == NodeType::Workspace {
        // Check workspace number via name parsing
        if let Some(name) = &node.name {
            if let Ok(num) = name.split(':').next().unwrap_or("").parse::<i32>() {
                if num == ws_num {
                    return Some(node);
                }
            }
        }
    }
    for child in &node.nodes {
        if let Some(found) = find_workspace(child, ws_num) {
            return Some(found);
        }
    }
    for child in &node.floating_nodes {
        if let Some(found) = find_workspace(child, ws_num) {
            return Some(found);
        }
    }
    None
}

fn is_workspace_empty(conn: &mut I3Connection, ws_num: i32) -> Result<bool> {
    let tree = conn.get_tree()?;
    if let Some(ws_node) = find_workspace(&tree, ws_num) {
        Ok(ws_node.nodes.is_empty() && ws_node.floating_nodes.is_empty())
    } else {
        Ok(true)
    }
}

fn has_i3mux_windows(node: &i3ipc::reply::Node) -> bool {
    // Check if this node is a window with an i3mux title
    if let Some(window_id) = node.window {
        if window_id != 0 {
            if let Some(name) = &node.name {
                if name.starts_with(MARKER) {
                    return true;
                }
            }
        }
    }

    // Recursively check child nodes
    for child in &node.nodes {
        if has_i3mux_windows(child) {
            return true;
        }
    }
    for child in &node.floating_nodes {
        if has_i3mux_windows(child) {
            return true;
        }
    }

    false
}

fn kill_i3mux_windows(conn: &mut I3Connection, node: &i3ipc::reply::Node) -> Result<()> {
    // Check if this node is a window with an i3mux title (starts with MARKER)
    if let Some(window_id) = node.window {
        if window_id != 0 {
            if let Some(name) = &node.name {
                if name.starts_with(MARKER) {
                    // This is an i3mux window - kill it
                    conn.run_command(&format!("[id=\"{}\"] kill", window_id))?;
                }
            }
        }
    }

    // Recursively check child nodes
    for child in &node.nodes {
        kill_i3mux_windows(conn, child)?;
    }
    for child in &node.floating_nodes {
        kill_i3mux_windows(conn, child)?;
    }

    Ok(())
}

fn find_focused_node(node: &i3ipc::reply::Node) -> Option<&i3ipc::reply::Node> {
    if node.focused {
        return Some(node);
    }
    for child in &node.nodes {
        if let Some(found) = find_focused_node(child) {
            return Some(found);
        }
    }
    for child in &node.floating_nodes {
        if let Some(found) = find_focused_node(child) {
            return Some(found);
        }
    }
    None
}

fn is_i3mux_terminal(conn: &mut I3Connection) -> Result<bool> {
    let tree = conn.get_tree()?;

    if let Some(focused) = find_focused_node(&tree) {
        if let Some(name) = &focused.name {
            return Ok(name.starts_with(MARKER) && name.ends_with(MARKER));
        }
    }

    Ok(false)
}

fn is_terminal_window(conn: &mut I3Connection) -> Result<bool> {
    let tree = conn.get_tree()?;

    if let Some(focused) = find_focused_node(&tree) {
        if let Some(props) = &focused.window_properties {
            use i3ipc::reply::WindowProperty;
            let terminal_classes = [
                "URxvt", "Alacritty", "kitty", "st", "xterm",
                "konsole", "gnome-terminal", "foot",
            ];

            if let Some(class) = props.get(&WindowProperty::Class) {
                return Ok(terminal_classes.iter().any(|tc| class.contains(tc)));
            }
        }
    }

    Ok(false)
}

fn get_terminal_command() -> String {
    std::env::var("TERMINAL").unwrap_or_else(|_| "i3-sensible-terminal".to_string())
}

fn get_user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string())
}

fn launch_normal_terminal() -> Result<()> {
    Command::new(get_terminal_command())
        .spawn()
        .context("Failed to launch terminal")?;
    Ok(())
}

fn launch_i3mux_terminal(ws_name: &str) -> Result<()> {
    debug!("launch_i3mux_terminal called for workspace: {}", ws_name);

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
            // Local: Set PROMPT_COMMAND to maintain title
            let prompt_cmd_val = format!("echo -ne \\\"\\\\033]0;{}\\\\007\\\"", title_for_prompt);
            format!(
                r#"bash -c "export PROMPT_COMMAND='{}'; exec abduco -A /tmp/{} {}""#,
                prompt_cmd_val, socket, user_shell
            )
        } else {
            // Remote: Use helper script to attach (ensures PATH is set correctly)
            format!(
                r#"TERM=xterm-256color ssh -o ControlPath=/tmp/i3mux/sockets/%r@%h:%p -o ControlMaster=auto -o ControlPersist=10m -t {} 'exec bash -lc "{} attach {}"'"#,
                ws_state.host, REMOTE_HELPER_PATH, socket
            )
        };

        // Cleanup script to run after terminal exits
        let cleanup_cmd = if let Some(session_name) = &ws_state.session_name {
            let ws_prefix = format!("ws{}", ws_name);
            if ws_state.session_type == "local" {
                // Local cleanup: check if any abduco sessions remain
                format!(
                    r#"
if ! abduco | grep -q '^{ws_prefix}-'; then
    rm -f /tmp/i3mux/sessions/{session}.json
    rm -f /tmp/i3mux/locks/{session}.lock
fi
                    "#,
                    ws_prefix = ws_prefix,
                    session = session_name
                )
            } else {
                // Remote cleanup: Use helper script to check and clean up
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

        (title, attach_cmd, cleanup_cmd)
    };

    state.save()?;

    let ws_state = state.workspaces.get(ws_name).unwrap();

    debug!("Session type: {}", ws_state.session_type);
    debug!("Host: {}", ws_state.host);
    debug!("Title: {}", title);
    debug!("Attach command: {}", attach_cmd);

    // Create wrapper script with debug logging
    // For remote sessions, set PROMPT_COMMAND to maintain title through SSH
    // For local sessions, just set title once at start
    let log_file = format!("/tmp/i3mux-{}.log", socket);
    let wrapper = if ws_state.session_type == "remote" {
        let title_escape = format!("\\033]0;{}\\007", title);
        format!(
            r#"exec &> >(tee -a {log}) 2>&1
echo "[i3mux wrapper] Starting at $(date)"
echo "[i3mux wrapper] Title: {title}"
echo "[i3mux wrapper] Socket: {socket}"
echo "[i3mux wrapper] Attach command: {attach}"
export PROMPT_COMMAND='echo -ne "{title_esc}"'
echo -ne '\033]0;{title}\007'
echo "[i3mux wrapper] Running attach command..."
{attach}
RC=$?
echo "[i3mux wrapper] Attach command exited with code: $RC"
{cleanup}
echo "[i3mux wrapper] Session ended at $(date)"
read -p "Press Enter to close terminal..." || true"#,
            log = log_file,
            title = title,
            socket = socket,
            attach = attach_cmd,
            title_esc = title_escape,
            cleanup = cleanup_cmd
        )
    } else {
        format!(
            r#"exec &> >(tee -a {log}) 2>&1
echo "[i3mux wrapper] Starting at $(date)"
echo "[i3mux wrapper] Title: {title}"
echo "[i3mux wrapper] Socket: {socket}"
echo "[i3mux wrapper] Attach command: {attach}"
echo -ne '\033]0;{title}\007'
echo "[i3mux wrapper] Running attach command..."
{attach}
RC=$?
echo "[i3mux wrapper] Attach command exited with code: $RC"
{cleanup}
echo "[i3mux wrapper] Session ended at $(date)"
read -p "Press Enter to close terminal..." || true"#,
            log = log_file,
            title = title,
            socket = socket,
            attach = attach_cmd,
            cleanup = cleanup_cmd
        )
    };

    debug!("Wrapper script: {}", wrapper);
    debug!("Terminal command: {}", get_terminal_command());

    Command::new(get_terminal_command())
        .arg("-T")
        .arg(&title)
        .arg("-e")
        .arg("bash")
        .arg("-c")
        .arg(&wrapper)
        .spawn()
        .context("Failed to launch i3mux terminal")?;

    debug!("Terminal spawned successfully");

    // Wait for window to appear and mark it
    // For SSH connections, this can take longer, so retry with backoff
    let mut conn = I3Connection::connect()?;
    let mut attempts = 0;
    let max_attempts = 20; // Up to 2 seconds (20 * 100ms)

    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Try to mark the window
        let mark_cmd = format!("[title=\"{}\"] mark --add i3mux-terminal", title);
        let result = conn.run_command(&mark_cmd)?;

        // Check if the command succeeded (window was found)
        if result.outcomes.iter().any(|o| o.success) {
            debug!("Successfully marked window '{}' after {} attempts", title, attempts + 1);
            break;
        }

        attempts += 1;
        if attempts >= max_attempts {
            debug!("ERROR: Failed to find window with title '{}' after {} attempts", title, attempts);
            anyhow::bail!("Failed to find window with title '{}' after {} attempts", title, attempts);
        }
    }

    debug!("launch_i3mux_terminal completed successfully");
    Ok(())
}

fn restore_layout(
    conn: &mut I3Connection,
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

        let attach_cmd = format!(
            r#"TERM=xterm-256color ssh -o ControlPath=/tmp/i3mux/sockets/%r@%h:%p -o ControlMaster=auto -o ControlPersist=10m -t {} 'exec bash -lc "{} attach {}"'"#,
            remote_host, REMOTE_HELPER_PATH, socket_id
        );

        let wrapper = format!(
            r#"echo -ne '\033]0;{}\007'; {}; echo 'Session ended.'"#,
            title, attach_cmd
        );

        Command::new(get_terminal_command())
            .arg("-T")
            .arg(&title)
            .arg("-e")
            .arg("bash")
            .arg("-c")
            .arg(&wrapper)
            .spawn()?;

        // Wait for window to appear with retry logic (SSH connections can be slow)
        let mut attempts = 0;
        let max_attempts = 20; // Up to 2 seconds

        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));

            let mark_cmd = format!("[title=\"{}\"] mark --add i3mux-terminal", title);
            let result = conn.run_command(&mark_cmd)?;

            if result.outcomes.iter().any(|o| o.success) {
                break;
            }

            attempts += 1;
            if attempts >= max_attempts {
                anyhow::bail!("Failed to find window with title '{}' during layout restore", title);
            }
        }

        // Execute layout command if available
        if i < commands.len() {
            conn.run_command(&commands[i])?;
        }
    }

    Ok(())
}
