//! I3mux window identification and management
//!
//! This module provides a single source of truth for identifying and managing
//! i3mux-managed windows. It uses i3 marks as the primary identification mechanism,
//! which is robust and fully under our control (unlike terminal-dependent approaches).
//!
//! ## Mark Format
//!
//! i3mux windows are marked with: `_i3mux:{host}:{socket}`
//!
//! - The underscore prefix makes the mark hidden (not shown in title bar)
//! - `host` is either "local" or the remote host identifier
//! - `socket` is the abduco socket name (e.g., "ws1-001")
//!
//! ## Example
//!
//! ```text
//! _i3mux:local:ws1-001
//! _i3mux:user@server:ws2-003
//! ```

use anyhow::{Context, Result};
use i3ipc::I3Connection;
use serde::{Deserialize, Serialize};
use std::process::Command;

/// Prefix for hidden i3 marks (underscore = hidden from title bar)
pub const MARK_PREFIX: &str = "_i3mux:";

/// Maximum attempts when waiting for a window to appear
pub const WINDOW_WAIT_MAX_ATTEMPTS: u32 = 30;

/// Polling interval when waiting for a window (milliseconds)
pub const WINDOW_WAIT_INTERVAL_MS: u64 = 100;

/// Represents an i3mux-managed window's identity
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct I3muxWindow {
    /// The i3 window ID
    pub window_id: u64,
    /// Host identifier ("local" or remote host like "user@server")
    pub host: String,
    /// Abduco socket name (e.g., "ws1-001")
    pub socket: String,
}

impl I3muxWindow {
    /// Create a new I3muxWindow identity
    pub fn new(window_id: u64, host: &str, socket: &str) -> Self {
        Self {
            window_id,
            host: host.to_string(),
            socket: socket.to_string(),
        }
    }

    /// Generate the i3 mark string for this window
    pub fn mark(&self) -> String {
        Self::mark_from_parts(&self.host, &self.socket)
    }

    /// Generate a mark/instance string from host and socket components
    ///
    /// The instance name and mark use the same format: `_i3mux:{host}:{socket}`
    pub fn mark_from_parts(host: &str, socket: &str) -> String {
        format!("{}{}:{}", MARK_PREFIX, host, socket)
    }

    /// Parse an i3mux identity from a mark string
    ///
    /// Returns None if the mark doesn't match the i3mux format
    pub fn from_mark(mark: &str) -> Option<Self> {
        if !mark.starts_with(MARK_PREFIX) {
            return None;
        }

        let data = mark.trim_start_matches(MARK_PREFIX);
        let parts: Vec<&str> = data.splitn(2, ':').collect();

        if parts.len() != 2 {
            return None;
        }

        Some(Self {
            window_id: 0, // Caller should fill this in
            host: parts[0].to_string(),
            socket: parts[1].to_string(),
        })
    }

    /// Check if a mark string identifies an i3mux window
    pub fn is_i3mux_mark(mark: &str) -> bool {
        mark.starts_with(MARK_PREFIX)
    }

    /// Apply the i3mux mark to a window
    ///
    /// This should be called after the window appears to mark it as i3mux-managed.
    pub fn apply_mark(&self, conn: &mut I3Connection) -> Result<()> {
        let mark = self.mark();
        let cmd = format!("[id=\"{}\"] mark --add {}", self.window_id, mark);
        let result = conn.run_command(&cmd)?;

        if !result.outcomes.iter().any(|o| o.success) {
            anyhow::bail!("Failed to apply mark '{}' to window {}", mark, self.window_id);
        }

        Ok(())
    }
}

/// Find a window by its WM_CLASS instance name
///
/// Searches the i3 tree for a window with the specified instance.
/// Returns the window ID if found.
pub fn find_window_by_instance(instance: &str) -> Option<u64> {
    let output = Command::new("i3-msg")
        .args(["-t", "get_tree"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    let tree: serde_json::Value = serde_json::from_str(&json_str).ok()?;

    find_window_by_instance_in_tree(&tree, instance)
}

fn find_window_by_instance_in_tree(node: &serde_json::Value, target_instance: &str) -> Option<u64> {
    // Check if this node has the target instance
    if let Some(window_id) = node.get("window").and_then(|w| w.as_u64()) {
        if let Some(props) = node.get("window_properties") {
            if let Some(instance) = props.get("instance").and_then(|i| i.as_str()) {
                if instance == target_instance {
                    return Some(window_id);
                }
            }
        }
    }

    // Recurse into children
    if let Some(nodes) = node.get("nodes").and_then(|n| n.as_array()) {
        for child in nodes {
            if let Some(found) = find_window_by_instance_in_tree(child, target_instance) {
                return Some(found);
            }
        }
    }

    if let Some(nodes) = node.get("floating_nodes").and_then(|n| n.as_array()) {
        for child in nodes {
            if let Some(found) = find_window_by_instance_in_tree(child, target_instance) {
                return Some(found);
            }
        }
    }

    None
}

/// Wait for a window to appear by instance name, then apply i3mux mark
///
/// Polls until the window appears or max_attempts is reached.
/// Returns the window ID on success.
pub fn wait_for_window_and_mark(
    conn: &mut I3Connection,
    instance: &str,
    host: &str,
    socket: &str,
) -> Result<u64> {
    for attempt in 0..WINDOW_WAIT_MAX_ATTEMPTS {
        std::thread::sleep(std::time::Duration::from_millis(WINDOW_WAIT_INTERVAL_MS));

        if let Some(window_id) = find_window_by_instance(instance) {
            let i3mux_window = I3muxWindow::new(window_id, host, socket);
            i3mux_window.apply_mark(conn)?;
            return Ok(window_id);
        }

        // Log progress at intervals
        if (attempt + 1) % 10 == 0 {
            eprintln!(
                "[i3mux] Still waiting for window with instance '{}' ({}/{})",
                instance, attempt + 1, WINDOW_WAIT_MAX_ATTEMPTS
            );
        }
    }

    anyhow::bail!(
        "Failed to find window with instance '{}' after {} attempts",
        instance,
        WINDOW_WAIT_MAX_ATTEMPTS
    )
}

/// Find all i3mux windows in a specific workspace
pub fn find_i3mux_windows_in_workspace(workspace_num: i32) -> Result<Vec<I3muxWindow>> {
    let output = Command::new("i3-msg")
        .args(["-t", "get_tree"])
        .output()
        .context("Failed to run i3-msg")?;

    if !output.status.success() {
        anyhow::bail!("i3-msg get_tree failed");
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    let tree: serde_json::Value = serde_json::from_str(&json_str)
        .context("Failed to parse i3 tree JSON")?;

    // Find the workspace node first
    let ws_node = find_workspace_node(&tree, workspace_num);

    match ws_node {
        Some(node) => {
            let mut windows = Vec::new();
            collect_i3mux_windows(node, &mut windows);
            Ok(windows)
        }
        None => Ok(Vec::new()),
    }
}

/// Kill all i3mux windows in a workspace
pub fn kill_i3mux_windows_in_workspace(conn: &mut I3Connection, workspace_num: i32) -> Result<()> {
    let windows = find_i3mux_windows_in_workspace(workspace_num)?;

    for window in windows {
        let cmd = format!("[id=\"{}\"] kill", window.window_id);
        let _ = conn.run_command(&cmd); // Ignore errors for individual windows
    }

    Ok(())
}

/// Check if a workspace has any i3mux windows
pub fn workspace_has_i3mux_windows(workspace_num: i32) -> Result<bool> {
    let windows = find_i3mux_windows_in_workspace(workspace_num)?;
    Ok(!windows.is_empty())
}

// ============ Internal helpers ============

fn collect_i3mux_windows(node: &serde_json::Value, windows: &mut Vec<I3muxWindow>) {
    // Check if this node has marks
    if let Some(marks) = node.get("marks").and_then(|m| m.as_array()) {
        if let Some(window_id) = node.get("window").and_then(|w| w.as_u64()) {
            for mark in marks {
                if let Some(mark_str) = mark.as_str() {
                    if let Some(mut identity) = I3muxWindow::from_mark(mark_str) {
                        identity.window_id = window_id;
                        windows.push(identity);
                        break; // Only count once per window
                    }
                }
            }
        }
    }

    // Recurse into children
    if let Some(nodes) = node.get("nodes").and_then(|n| n.as_array()) {
        for child in nodes {
            collect_i3mux_windows(child, windows);
        }
    }

    if let Some(nodes) = node.get("floating_nodes").and_then(|n| n.as_array()) {
        for child in nodes {
            collect_i3mux_windows(child, windows);
        }
    }
}

fn find_workspace_node<'a>(node: &'a serde_json::Value, workspace_num: i32) -> Option<&'a serde_json::Value> {
    // Check if this is the workspace we're looking for
    if let Some(node_type) = node.get("type").and_then(|t| t.as_str()) {
        if node_type == "workspace" {
            if let Some(num) = node.get("num").and_then(|n| n.as_i64()) {
                if num == workspace_num as i64 {
                    return Some(node);
                }
            }
        }
    }

    // Recurse into children
    if let Some(nodes) = node.get("nodes").and_then(|n| n.as_array()) {
        for child in nodes {
            if let Some(found) = find_workspace_node(child, workspace_num) {
                return Some(found);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mark_format() {
        let window = I3muxWindow::new(12345, "local", "ws1-001");
        assert_eq!(window.mark(), "_i3mux:local:ws1-001");
    }

    #[test]
    fn test_mark_format_remote() {
        let window = I3muxWindow::new(12345, "user@server", "ws2-003");
        assert_eq!(window.mark(), "_i3mux:user@server:ws2-003");
    }

    #[test]
    fn test_from_mark_local() {
        let identity = I3muxWindow::from_mark("_i3mux:local:ws1-001").unwrap();
        assert_eq!(identity.host, "local");
        assert_eq!(identity.socket, "ws1-001");
    }

    #[test]
    fn test_from_mark_remote() {
        let identity = I3muxWindow::from_mark("_i3mux:user@server:ws2-003").unwrap();
        assert_eq!(identity.host, "user@server");
        assert_eq!(identity.socket, "ws2-003");
    }

    #[test]
    fn test_from_mark_invalid() {
        assert!(I3muxWindow::from_mark("random-mark").is_none());
        assert!(I3muxWindow::from_mark("i3mux:local:ws1").is_none()); // Missing underscore
        assert!(I3muxWindow::from_mark("_i3mux:nocolon").is_none());
    }

    #[test]
    fn test_is_i3mux_mark() {
        assert!(I3muxWindow::is_i3mux_mark("_i3mux:local:ws1-001"));
        assert!(I3muxWindow::is_i3mux_mark("_i3mux:user@host:ws2-003"));
        assert!(!I3muxWindow::is_i3mux_mark("random-mark"));
        assert!(!I3muxWindow::is_i3mux_mark("i3mux-terminal"));
    }
}
