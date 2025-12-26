//! Window Manager backend abstraction for i3 and Sway support
//!
//! This module provides a unified interface for interacting with i3 or Sway,
//! automatically detecting which window manager is running at startup.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::process::Command;

/// Detected window manager type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmType {
    I3,
    Sway,
}

/// Window manager backend abstraction
pub struct WmBackend {
    wm_type: WmType,
    socket_path: String,
}

/// Workspace information from the window manager
#[derive(Debug, Deserialize)]
pub struct WorkspaceInfo {
    pub num: i32,
    pub name: String,
    pub focused: bool,
}

impl WmBackend {
    /// Detect and connect to the running window manager
    ///
    /// Checks for Sway first (SWAYSOCK), then i3 (I3SOCK).
    /// Falls back to querying the WM directly if env vars are not set.
    pub fn connect() -> Result<Self> {
        // Try Sway first (SWAYSOCK)
        if let Ok(socket) = std::env::var("SWAYSOCK") {
            return Ok(Self {
                wm_type: WmType::Sway,
                socket_path: socket,
            });
        }

        // Then try i3 (I3SOCK)
        if let Ok(socket) = std::env::var("I3SOCK") {
            return Ok(Self {
                wm_type: WmType::I3,
                socket_path: socket,
            });
        }

        // Fallback: try to get socket path from sway
        if let Ok(output) = Command::new("sway").arg("--get-socketpath").output() {
            if output.status.success() {
                let socket = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !socket.is_empty() {
                    return Ok(Self {
                        wm_type: WmType::Sway,
                        socket_path: socket,
                    });
                }
            }
        }

        // Fallback: try to get socket path from i3
        if let Ok(output) = Command::new("i3").arg("--get-socketpath").output() {
            if output.status.success() {
                let socket = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !socket.is_empty() {
                    return Ok(Self {
                        wm_type: WmType::I3,
                        socket_path: socket,
                    });
                }
            }
        }

        anyhow::bail!("No running window manager (i3 or Sway) detected. Ensure I3SOCK or SWAYSOCK is set.")
    }

    /// Get the window manager type
    pub fn wm_type(&self) -> WmType {
        self.wm_type
    }

    /// Get the CLI command name for this WM
    fn msg_command(&self) -> &'static str {
        match self.wm_type {
            WmType::I3 => "i3-msg",
            WmType::Sway => "swaymsg",
        }
    }

    /// Run a WM command (like "split h", "kill", etc.)
    ///
    /// Returns Ok(()) if the command was executed. Note that some commands
    /// may "succeed" from the WM's perspective even if they don't match any windows.
    pub fn run_command(&self, cmd: &str) -> Result<()> {
        let output = Command::new(self.msg_command())
            .args(["-s", &self.socket_path, cmd])
            .output()
            .with_context(|| format!("Failed to run {} command", self.msg_command()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("{} command failed: {}", self.msg_command(), stderr.trim());
        }

        Ok(())
    }

    /// Get the i3/sway tree as JSON
    pub fn get_tree(&self) -> Result<Value> {
        let output = Command::new(self.msg_command())
            .args(["-s", &self.socket_path, "-t", "get_tree"])
            .output()
            .with_context(|| format!("Failed to get {} tree", self.msg_command()))?;

        if !output.status.success() {
            anyhow::bail!("{} get_tree failed", self.msg_command());
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str(&json_str).context("Failed to parse WM tree JSON")
    }

    /// Get list of workspaces
    pub fn get_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let output = Command::new(self.msg_command())
            .args(["-s", &self.socket_path, "-t", "get_workspaces"])
            .output()
            .with_context(|| format!("Failed to get {} workspaces", self.msg_command()))?;

        if !output.status.success() {
            anyhow::bail!("{} get_workspaces failed", self.msg_command());
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str(&json_str).context("Failed to parse workspaces JSON")
    }

    /// Run a command targeting a specific window by container ID
    ///
    /// Uses the `[con_id="..."]` selector which works for both i3 and Sway.
    pub fn run_command_on_container(&self, container_id: u64, cmd: &str) -> Result<()> {
        let full_cmd = format!("[con_id=\"{}\"] {}", container_id, cmd);
        self.run_command(&full_cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wm_type_debug() {
        assert_eq!(format!("{:?}", WmType::I3), "I3");
        assert_eq!(format!("{:?}", WmType::Sway), "Sway");
    }
}
