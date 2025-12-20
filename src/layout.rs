//! Layout capture and restoration for i3mux
//!
//! This module handles capturing the current i3 layout structure and
//! serializing/deserializing it for session persistence.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::process::Command;

use crate::window::I3muxWindow;

/// Simplified i3 layout representation for serialization
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum Layout {
    /// Horizontal split container
    #[serde(rename = "hsplit")]
    HSplit {
        children: Vec<Layout>,
        #[serde(skip_serializing_if = "Option::is_none")]
        percent: Option<f64>,
    },
    /// Vertical split container
    #[serde(rename = "vsplit")]
    VSplit {
        children: Vec<Layout>,
        #[serde(skip_serializing_if = "Option::is_none")]
        percent: Option<f64>,
    },
    /// Tabbed container
    #[serde(rename = "tabbed")]
    Tabbed {
        children: Vec<Layout>,
    },
    /// Stacked container
    #[serde(rename = "stacked")]
    Stacked {
        children: Vec<Layout>,
    },
    /// i3mux terminal window (leaf)
    #[serde(rename = "terminal")]
    Terminal {
        socket: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        percent: Option<f64>,
    },
}

impl Layout {
    /// Capture layout from i3 workspace by number
    ///
    /// This uses i3-msg directly to get the tree and identify i3mux windows
    /// by their marks (the most reliable identification method).
    pub fn capture_from_workspace_num(workspace_num: i32) -> Result<Option<Self>> {
        let output = Command::new("i3-msg")
            .args(["-t", "get_tree"])
            .output()?;

        if !output.status.success() {
            anyhow::bail!("i3-msg get_tree failed");
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        let tree: serde_json::Value = serde_json::from_str(&json_str)?;

        // Find the workspace node
        let ws_node = find_workspace_node(&tree, workspace_num);

        match ws_node {
            Some(node) => capture_node_from_json(node),
            None => Ok(None),
        }
    }

    /// Get list of all socket IDs in this layout
    pub fn get_sockets(&self) -> Vec<String> {
        match self {
            Layout::Terminal { socket, .. } => vec![socket.clone()],
            Layout::HSplit { children, .. }
            | Layout::VSplit { children, .. }
            | Layout::Tabbed { children }
            | Layout::Stacked { children } => {
                children.iter().flat_map(|c| c.get_sockets()).collect()
            }
        }
    }

    /// Generate i3 commands to recreate this layout
    pub fn generate_i3_commands(&self, depth: usize) -> Vec<String> {
        let mut commands = Vec::new();

        match self {
            Layout::Terminal { .. } => {
                // Terminal will be launched separately
            }
            Layout::HSplit { children, .. } => {
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        commands.push("split h".to_string());
                    }
                    commands.extend(child.generate_i3_commands(depth + 1));
                }
            }
            Layout::VSplit { children, .. } => {
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        commands.push("split v".to_string());
                    }
                    commands.extend(child.generate_i3_commands(depth + 1));
                }
            }
            Layout::Tabbed { children } => {
                if depth > 0 {
                    commands.push("layout tabbed".to_string());
                }
                for child in children {
                    commands.extend(child.generate_i3_commands(depth + 1));
                }
            }
            Layout::Stacked { children } => {
                if depth > 0 {
                    commands.push("layout stacking".to_string());
                }
                for child in children {
                    commands.extend(child.generate_i3_commands(depth + 1));
                }
            }
        }

        commands
    }
}

// ============ Internal JSON-based capture (uses marks) ============

fn capture_node_from_json(node: &serde_json::Value) -> Result<Option<Layout>> {
    // Check if this node is an i3mux terminal by looking at marks
    if let Some(marks) = node.get("marks").and_then(|m| m.as_array()) {
        for mark in marks {
            if let Some(mark_str) = mark.as_str() {
                if let Some(identity) = I3muxWindow::from_mark(mark_str) {
                    // This is an i3mux terminal
                    let percent = node.get("percent").and_then(|p| p.as_f64());
                    return Ok(Some(Layout::Terminal {
                        socket: identity.socket,
                        percent,
                    }));
                }
            }
        }
    }

    // Not a terminal, check if it's a container with i3mux children
    let mut children = Vec::new();

    // Check regular nodes
    if let Some(nodes) = node.get("nodes").and_then(|n| n.as_array()) {
        for child in nodes {
            if let Some(layout) = capture_node_from_json(child)? {
                children.push(layout);
            }
        }
    }

    // Check floating nodes
    if let Some(nodes) = node.get("floating_nodes").and_then(|n| n.as_array()) {
        for child in nodes {
            if let Some(layout) = capture_node_from_json(child)? {
                children.push(layout);
            }
        }
    }

    if children.is_empty() {
        return Ok(None);
    }

    // Determine container type from layout
    let layout_type = node.get("layout").and_then(|l| l.as_str()).unwrap_or("splith");
    let percent = node.get("percent").and_then(|p| p.as_f64());

    let layout = match layout_type {
        "splith" => Layout::HSplit { children, percent },
        "splitv" => Layout::VSplit { children, percent },
        "tabbed" => Layout::Tabbed { children },
        "stacked" => Layout::Stacked { children },
        _ => Layout::VSplit { children, percent }, // Default
    };

    Ok(Some(layout))
}

fn find_workspace_node(node: &serde_json::Value, workspace_num: i32) -> Option<&serde_json::Value> {
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
    fn test_get_sockets() {
        let layout = Layout::HSplit {
            children: vec![
                Layout::Terminal {
                    socket: "ws4-001".to_string(),
                    percent: Some(0.5),
                },
                Layout::VSplit {
                    children: vec![
                        Layout::Terminal {
                            socket: "ws4-002".to_string(),
                            percent: Some(0.5),
                        },
                        Layout::Terminal {
                            socket: "ws4-003".to_string(),
                            percent: Some(0.5),
                        },
                    ],
                    percent: Some(0.5),
                },
            ],
            percent: None,
        };

        let sockets = layout.get_sockets();
        assert_eq!(sockets, vec!["ws4-001", "ws4-002", "ws4-003"]);
    }
}
