use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::connection::Connection;
use crate::layout::Layout;

/// Remote session state stored on the remote host
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RemoteSession {
    pub name: String,
    pub workspace: String,
    pub host: String,
    pub layout: Layout,
    pub lock: Option<SessionLock>,
}

/// Server-side lock maintained by SSH daemon
/// Lock file exists on remote as long as SSH connection is alive
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SessionLock {
    /// Hostname that created the lock
    pub locked_by: String,

    /// When lock was created
    pub locked_at: String,

    /// Unique nonce for this attach session
    pub nonce: String,

    /// PID of the lock-holding process on the remote (for validation)
    pub remote_pid: u32,
}

impl SessionLock {
    pub fn new(hostname: String, remote_pid: u32) -> Self {
        let nonce = uuid::Uuid::new_v4().to_string();

        Self {
            locked_by: hostname,
            locked_at: chrono::Utc::now().to_rfc3339(),
            nonce,
            remote_pid,
        }
    }
}

impl RemoteSession {
    pub fn new(name: String, workspace: String, host: String, layout: Layout) -> Result<Self> {
        Ok(Self {
            name,
            workspace,
            host,
            layout,
            lock: None,
        })
    }

    /// Save session to remote host
    pub fn save_to_remote(&self, conn: &dyn Connection) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        conn.save_session_data(&self.name, &json)
    }

    /// Load session from remote host
    pub fn load_from_remote(conn: &dyn Connection, name: &str) -> Result<Self> {
        let content = conn.load_session_data(name)?;
        let session: RemoteSession = serde_json::from_str(&content)
            .context("Failed to parse session file")?;
        Ok(session)
    }

    /// List all sessions on remote host
    pub fn list_remote_sessions(conn: &dyn Connection) -> Result<Vec<String>> {
        conn.list_session_names()
    }
}
