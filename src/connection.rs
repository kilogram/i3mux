use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

use crate::session::SessionLock;

const BASE_DIR: &str = "/tmp/i3mux";

/// High-level abstraction for managing sessions and terminals on local or remote hosts
pub trait Connection: Send + Sync {
    // Session persistence
    fn save_session_data(&self, name: &str, data: &str) -> Result<()>;
    fn load_session_data(&self, name: &str) -> Result<String>;
    fn list_session_names(&self) -> Result<Vec<String>>;

    // Lock management (connection-specific strategy)
    fn acquire_lock(&self, session_name: &str, force: bool) -> Result<(SessionLock, Option<std::process::Child>)>;
    fn is_lock_valid(&self, lock: &SessionLock) -> Result<bool>;
    fn release_lock(&self, session_name: &str) -> Result<()>;

    // Session deletion
    fn delete_session(&self, name: &str) -> Result<()>;
}

/// Local connection (executes commands directly on localhost)
pub struct LocalConnection;

impl LocalConnection {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    fn sessions_dir() -> PathBuf {
        PathBuf::from(BASE_DIR).join("sessions")
    }

    fn locks_dir() -> PathBuf {
        PathBuf::from(BASE_DIR).join("locks")
    }

    fn check(&self, cmd: &str) -> Result<bool> {
        let status = Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .status()
            .context("Failed to execute local command")?;

        Ok(status.success())
    }
}

impl Default for LocalConnection {
    fn default() -> Self {
        Self::new().expect("Failed to initialize LocalConnection")
    }
}

impl Connection for LocalConnection {
    fn save_session_data(&self, name: &str, data: &str) -> Result<()> {
        let dir = Self::sessions_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", name));
        std::fs::write(&path, data)
            .with_context(|| format!("Failed to write session file: {}", path.display()))
    }

    fn load_session_data(&self, name: &str) -> Result<String> {
        let path = Self::sessions_dir().join(format!("{}.json", name));
        std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to load session '{}' from {}", name, path.display()))
    }

    fn list_session_names(&self) -> Result<Vec<String>> {
        let dir = Self::sessions_dir();
        let mut sessions = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.ends_with(".json") {
                        sessions.push(name.trim_end_matches(".json").to_string());
                    }
                }
            }
        }

        Ok(sessions)
    }

    fn delete_session(&self, name: &str) -> Result<()> {
        let path = Self::sessions_dir().join(format!("{}.json", name));
        match std::fs::remove_file(&path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("Failed to delete session file: {}", path.display())),
        }
    }

    fn acquire_lock(&self, session_name: &str, force: bool) -> Result<(SessionLock, Option<std::process::Child>)> {
        let hostname = gethostname::gethostname()
            .into_string()
            .unwrap_or_else(|_| "unknown".to_string());

        let locks_dir = Self::locks_dir();
        let lock_path = locks_dir.join(format!("{}.lock", session_name));

        // Check if lock already exists
        if !force {
            if let Ok(lock_content) = std::fs::read_to_string(&lock_path) {
                if let Ok(lock) = serde_json::from_str::<SessionLock>(&lock_content) {
                    if self.is_lock_valid(&lock)? {
                        anyhow::bail!(
                            "Session '{}' is locked by {} (acquired {}). Use --force to break lock.",
                            session_name,
                            lock.locked_by,
                            lock.locked_at
                        );
                    }
                }
            }
        }

        // For local: simple lockfile with PID
        let pid = std::process::id();
        let lock = SessionLock::new(hostname, pid);

        // Write lock file
        std::fs::create_dir_all(&locks_dir)?;
        let lock_json = serde_json::to_string(&lock)?;
        std::fs::write(&lock_path, &lock_json)
            .with_context(|| format!("Failed to write lock file: {}", lock_path.display()))?;

        // No background process needed for local locks
        Ok((lock, None))
    }

    fn is_lock_valid(&self, lock: &SessionLock) -> Result<bool> {
        // Check if process still exists
        self.check(&format!("kill -0 {} 2>/dev/null", lock.remote_pid))
    }

    fn release_lock(&self, session_name: &str) -> Result<()> {
        let lock_path = Self::locks_dir().join(format!("{}.lock", session_name));
        match std::fs::remove_file(&lock_path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("Failed to release lock: {}", lock_path.display())),
        }
    }
}

/// SSH connection (executes commands via SSH with ControlMaster)
pub struct SshConnection {
    host: String,
}

impl SshConnection {
    pub fn new(host: String) -> Self {
        Self { host }
    }

    // Private helper methods
    fn ssh_base_args(&self) -> Vec<String> {
        vec![
            "-o".to_string(),
            "ControlPath=/tmp/i3mux/sockets/%r@%h:%p".to_string(),
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            "ControlPersist=10m".to_string(),
        ]
    }

    fn execute(&self, cmd: &str) -> Result<String> {
        let mut command = Command::new("ssh");
        for arg in self.ssh_base_args() {
            command.arg(arg);
        }
        command.arg(&self.host).arg(cmd);

        let output = command.output().context("Failed to execute SSH command")?;

        if !output.status.success() {
            anyhow::bail!(
                "SSH command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn check(&self, cmd: &str) -> Result<bool> {
        let mut command = Command::new("ssh");
        for arg in self.ssh_base_args() {
            command.arg(arg);
        }
        command.arg(&self.host).arg(cmd);

        let status = command.status().context("Failed to execute SSH command")?;
        Ok(status.success())
    }

    fn write_remote_file(&self, path: &str, content: &str) -> Result<()> {
        let mut command = Command::new("ssh");
        for arg in self.ssh_base_args() {
            command.arg(arg);
        }
        command
            .arg(&self.host)
            .arg(format!("cat > {}", path))
            .stdin(std::process::Stdio::piped());

        let mut child = command.spawn().context("Failed to start SSH write")?;

        use std::io::Write;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(content.as_bytes())
                .context("Failed to write to SSH stdin")?;
        }

        child.wait().context("Failed to wait for SSH write")?;
        Ok(())
    }
}

impl Connection for SshConnection {
    fn save_session_data(&self, name: &str, data: &str) -> Result<()> {
        let path = format!("{}/sessions/{}.json", BASE_DIR, name);
        // Ensure parent directory exists
        self.execute(&format!("mkdir -p {}/sessions", BASE_DIR))?;
        self.write_remote_file(&path, data)
    }

    fn load_session_data(&self, name: &str) -> Result<String> {
        let path = format!("{}/sessions/{}.json", BASE_DIR, name);
        self.execute(&format!("cat '{}'", path))
            .with_context(|| format!("Session '{}' not found on {}", name, self.host))
    }

    fn list_session_names(&self) -> Result<Vec<String>> {
        let output = self.execute(&format!(
            "ls {}/sessions/*.json 2>/dev/null | xargs -n1 basename -s .json || true",
            BASE_DIR
        ))?;
        Ok(output
            .lines()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect())
    }

    fn delete_session(&self, name: &str) -> Result<()> {
        let path = format!("{}/sessions/{}.json", BASE_DIR, name);
        self.execute(&format!("rm -f '{}'", path))?;
        Ok(())
    }

    fn acquire_lock(&self, session_name: &str, force: bool) -> Result<(SessionLock, Option<std::process::Child>)> {
        let hostname = gethostname::gethostname()
            .into_string()
            .unwrap_or_else(|_| "unknown".to_string());

        let lock_file = format!("{}/locks/{}.lock", BASE_DIR, session_name);
        let pid_file = format!("{}/locks/{}.lock.pid", BASE_DIR, session_name);

        // Check if lock already exists
        if !force {
            let pid_str = self.execute(&format!("cat '{}' 2>/dev/null || echo ''", pid_file))?;
            if !pid_str.trim().is_empty() {
                if let Ok(remote_pid) = pid_str.trim().parse::<u32>() {
                    if self.check(&format!("kill -0 {} 2>/dev/null", remote_pid))? {
                        // Lock still valid - try to load session for better error message
                        if let Ok(session_data) = self.load_session_data(session_name) {
                            if let Ok(session) = serde_json::from_str::<crate::session::RemoteSession>(&session_data) {
                                if let Some(lock) = session.lock {
                                    anyhow::bail!(
                                        "Session '{}' is locked by {} (acquired {}). Use --force to break lock.",
                                        session_name,
                                        lock.locked_by,
                                        lock.locked_at
                                    );
                                }
                            }
                        }
                        anyhow::bail!("Session '{}' is locked. Use --force to break lock.", session_name);
                    }
                }
            }
        }

        // Ensure lock directory exists
        self.execute(&format!("mkdir -p {}/locks", BASE_DIR))?;

        // Start background SSH process that holds the lock
        let lock_script = format!(
            r#"
            set -e
            LOCKFILE='{lock_file}'
            PIDFILE='{pid_file}'
            echo $$ > "$PIDFILE"
            trap "rm -f '$LOCKFILE' '$PIDFILE'" EXIT
            echo "Lock acquired by {hostname}" > "$LOCKFILE"

            while true; do
                sleep 30
                echo "heartbeat $(date +%s)" >> "$LOCKFILE"
            done
            "#,
            lock_file = lock_file,
            pid_file = pid_file,
            hostname = hostname
        );

        let mut command = Command::new("ssh");
        for arg in self.ssh_base_args() {
            command.arg(arg);
        }
        command
            .arg(&self.host)
            .arg("bash")
            .arg("-c")
            .arg(&lock_script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let child = command
            .spawn()
            .context("Failed to start lock holder process")?;

        std::thread::sleep(std::time::Duration::from_millis(500));

        let pid_str = self.execute(&format!("cat '{}' 2>/dev/null || echo 0", pid_file))?;
        let remote_pid: u32 = pid_str.trim().parse().unwrap_or(0);

        if remote_pid == 0 {
            anyhow::bail!("Failed to acquire lock - could not get remote PID");
        }

        let lock = SessionLock::new(hostname, remote_pid);
        Ok((lock, Some(child)))
    }

    fn is_lock_valid(&self, lock: &SessionLock) -> Result<bool> {
        self.check(&format!("kill -0 {} 2>/dev/null", lock.remote_pid))
    }

    fn release_lock(&self, session_name: &str) -> Result<()> {
        let lock_file = format!("{}/locks/{}.lock", BASE_DIR, session_name);
        let pid_file = format!("{}/locks/{}.lock.pid", BASE_DIR, session_name);

        self.execute(&format!(
            "test -f '{pid_file}' && kill $(cat '{pid_file}') 2>/dev/null; rm -f '{lock_file}' '{pid_file}'",
            pid_file = pid_file,
            lock_file = lock_file
        ))?;
        Ok(())
    }
}

/// Create a connection from an optional host string
/// None means local, Some(host) means remote SSH connection
pub fn create_connection(host: Option<&str>) -> Result<Box<dyn Connection>> {
    match host {
        None => Ok(Box::new(LocalConnection::new()?)),
        Some(h) => Ok(Box::new(SshConnection::new(h.to_string()))),
    }
}
