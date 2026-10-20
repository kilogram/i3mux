//! Sway IPC through `swaymsg`. The only module that runs Sway commands.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::sway_ops::Sway;
use crate::types::SwayInstance;

/// The kernel's random id of the running boot.
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

/// Talking to Sway failed.
#[derive(Debug, thiserror::Error)]
pub enum WmError {
    #[error("Sway is not running: SWAYSOCK is unset and 'sway --get-socketpath' found no socket")]
    NoSway,
    #[error("cannot run swaymsg: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("swaymsg {request} failed: {detail}")]
    Failed { request: String, detail: String },
    #[error("cannot parse the reply to swaymsg {request}: {source}")]
    BadReply {
        request: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("Sway reports no focused workspace")]
    NoFocusedWorkspace,
    #[error("cannot read the boot id from {BOOT_ID}: {0}")]
    BootId(#[source] std::io::Error),
}

/// A workspace as `swaymsg -t get_workspaces` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WorkspaceInfo {
    name: String,
    num: i32,
    focused: bool,
}

impl WorkspaceInfo {
    /// The workspace name; unique in Sway.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The workspace number; `None` for unnumbered workspaces (Sway uses -1).
    #[must_use]
    pub fn number(&self) -> Option<u32> {
        u32::try_from(self.num).ok()
    }
}

/// A connection to the running Sway, by IPC socket path.
#[derive(Debug, Clone)]
pub struct Wm {
    socket: PathBuf,
}

impl Wm {
    /// Find Sway's socket: `$SWAYSOCK`, else `sway --get-socketpath`.
    ///
    /// # Errors
    /// [`WmError::NoSway`] if neither finds a socket.
    pub fn connect() -> Result<Self, WmError> {
        if let Some(socket) = std::env::var_os("SWAYSOCK").filter(|s| !s.is_empty()) {
            return Ok(Self {
                socket: socket.into(),
            });
        }
        let output = Command::new("sway")
            .arg("--get-socketpath")
            .output()
            .map_err(|_| WmError::NoSway)?;
        let socket = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || socket.is_empty() {
            return Err(WmError::NoSway);
        }
        Ok(Self {
            socket: socket.into(),
        })
    }

    /// The running Sway: this boot's id and Sway's socket
    /// (plan-layout-backup decision 2).
    ///
    /// # Errors
    /// [`WmError::BootId`] if the boot id cannot be read.
    pub fn instance(&self) -> Result<SwayInstance, WmError> {
        let boot_id = std::fs::read_to_string(BOOT_ID).map_err(WmError::BootId)?;
        Ok(SwayInstance::new(
            boot_id.trim(),
            self.socket.to_string_lossy(),
        ))
    }

    /// Sway's IPC socket.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Subscribe to Sway's events: `swaymsg -r -m -t subscribe EVENTS`, where
    /// `events_json` is a JSON array of event types, such as
    /// `["window"]`. The child prints the reply to the subscription, then
    /// one JSON value per event on its stdout, until Sway closes its socket.
    /// Only the layout watcher subscribes.
    ///
    /// # Errors
    /// [`WmError::Spawn`] if `swaymsg` cannot start.
    pub fn subscribe(&self, events_json: &str) -> Result<Child, WmError> {
        Command::new("swaymsg")
            .arg("-s")
            .arg(&self.socket)
            .args(["-r", "-m", "-t", "subscribe", "--", events_json])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(WmError::Spawn)
    }

    /// Run `swaymsg -s SOCKET <args>` and return stdout.
    fn swaymsg(&self, request: &str, args: &[&str]) -> Result<String, WmError> {
        let output = Command::new("swaymsg")
            .arg("-s")
            .arg(&self.socket)
            .args(args)
            .output()
            .map_err(WmError::Spawn)?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            return Ok(stdout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(WmError::Failed {
            request: request.to_owned(),
            detail: format!("{} {}", stderr.trim(), stdout.trim())
                .trim()
                .to_owned(),
        })
    }

    fn query<T: serde::de::DeserializeOwned>(&self, kind: &str) -> Result<T, WmError> {
        let request = format!("-t {kind}");
        let stdout = self.swaymsg(&request, &["-t", kind])?;
        serde_json::from_str(&stdout).map_err(|source| WmError::BadReply { request, source })
    }

    /// All workspaces.
    ///
    /// # Errors
    /// Any [`WmError`].
    pub fn get_workspaces(&self) -> Result<Vec<WorkspaceInfo>, WmError> {
        self.query("get_workspaces")
    }

    /// The focused workspace.
    ///
    /// # Errors
    /// Any [`WmError`].
    pub fn focused_workspace(&self) -> Result<WorkspaceInfo, WmError> {
        self.get_workspaces()?
            .into_iter()
            .find(|ws| ws.focused)
            .ok_or(WmError::NoFocusedWorkspace)
    }
}

impl Sway for Wm {
    fn get_tree(&self) -> Result<Value> {
        Ok(self.query("get_tree")?)
    }

    fn run(&self, command: &str) -> Result<()> {
        self.swaymsg(&format!("'{command}'"), &["--", command])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_numbers() {
        let parse = |json| serde_json::from_str::<WorkspaceInfo>(json).unwrap();
        assert_eq!(
            parse(r#"{"name":"4","num":4,"focused":true}"#).number(),
            Some(4)
        );
        assert_eq!(
            parse(r#"{"name":"q","num":-1,"focused":true}"#).number(),
            None
        );
    }

    #[test]
    fn workspaces_parse_from_swaymsg_json() {
        let json = r#"[{"num":3,"name":"3","focused":false,"visible":true},
                       {"num":-1,"name":"q","focused":true,"output":"eDP-1"}]"#;
        let workspaces: Vec<WorkspaceInfo> = serde_json::from_str(json).unwrap();
        let focused = workspaces.iter().find(|w| w.focused).unwrap();
        assert_eq!(focused.name(), "q");
        assert_eq!(focused.number(), None);
    }
}
