use anyhow::Result;
use i3ipc::reply::Node;
use serde::{Deserialize, Serialize};
use std::process::Command;

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

const MARKER: &str = "i3mux:";

impl Layout {
    /// Capture layout from i3 workspace tree
    pub fn capture_from_workspace(workspace_node: &Node) -> Result<Option<Self>> {
        Self::capture_node(workspace_node)
    }

    fn capture_node(node: &Node) -> Result<Option<Self>> {
        use i3ipc::reply::WindowProperty;

        // Check if this is an i3mux terminal by looking at window instance name
        // (more reliable than title which can be changed by shell PS1/PROMPT_COMMAND)

        // First try i3ipc's window_properties
        let instance = if let Some(props) = &node.window_properties {
            props.get(&WindowProperty::Instance).cloned()
        } else if let Some(window_id) = node.window {
            // Fallback: i3ipc returns None for window_properties when i3 includes
            // unknown property keys (like "machine"). Use i3-msg directly as workaround.
            get_window_instance(window_id as u64)
        } else {
            None
        };

        if let Some(instance) = instance {
            if instance.starts_with(MARKER) {
                // Extract socket ID from instance: "i3mux:host:socket"
                let clean_name = instance.trim_start_matches(MARKER);
                if let Some(socket_part) = clean_name.split(':').nth(1) {
                    return Ok(Some(Layout::Terminal {
                        socket: socket_part.to_string(),
                        percent: node.percent,
                    }));
                }
            }
        }

        // Fallback: also check title for backwards compatibility
        if let Some(name) = &node.name {
            if name.starts_with(MARKER) {
                let clean_name = name.trim_start_matches(MARKER);
                if let Some(socket_part) = clean_name.split(':').nth(1) {
                    return Ok(Some(Layout::Terminal {
                        socket: socket_part.to_string(),
                        percent: node.percent,
                    }));
                }
            }
        }

        // Not a terminal, check if it's a container with i3mux children
        let children: Vec<Layout> = node
            .nodes
            .iter()
            .chain(node.floating_nodes.iter())
            .filter_map(|child| Self::capture_node(child).ok().flatten())
            .collect();

        if children.is_empty() {
            return Ok(None);
        }

        // Determine container type from layout
        use i3ipc::reply::NodeLayout;
        let layout = match node.layout {
            NodeLayout::SplitH => Layout::HSplit {
                children,
                percent: node.percent,
            },
            NodeLayout::SplitV => Layout::VSplit {
                children,
                percent: node.percent,
            },
            NodeLayout::Tabbed => Layout::Tabbed { children },
            NodeLayout::Stacked => Layout::Stacked { children },
            _ => {
                // Default to vsplit if unknown
                Layout::VSplit {
                    children,
                    percent: node.percent,
                }
            }
        };

        Ok(Some(layout))
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

/// Get window instance name using i3-msg directly (workaround for i3ipc crate bug)
/// The i3ipc crate returns None for window_properties if any unknown property key exists
fn get_window_instance(window_id: u64) -> Option<String> {
    // Run i3-msg -t get_tree and extract instance for this window
    let output = Command::new("i3-msg")
        .args(["-t", "get_tree"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json_str = String::from_utf8_lossy(&output.stdout);

    // Parse JSON and find the window with matching ID
    let tree: serde_json::Value = serde_json::from_str(&json_str).ok()?;

    find_window_instance(&tree, window_id)
}

fn find_window_instance(node: &serde_json::Value, window_id: u64) -> Option<String> {
    // Check if this node has the window we're looking for
    if let Some(window) = node.get("window").and_then(|w| w.as_u64()) {
        if window == window_id {
            // Found the window, get its instance from window_properties
            if let Some(props) = node.get("window_properties") {
                if let Some(instance) = props.get("instance").and_then(|i| i.as_str()) {
                    return Some(instance.to_string());
                }
            }
        }
    }

    // Recursively search nodes
    if let Some(nodes) = node.get("nodes").and_then(|n| n.as_array()) {
        for child in nodes {
            if let Some(instance) = find_window_instance(child, window_id) {
                return Some(instance);
            }
        }
    }

    // Also search floating_nodes
    if let Some(nodes) = node.get("floating_nodes").and_then(|n| n.as_array()) {
        for child in nodes {
            if let Some(instance) = find_window_instance(child, window_id) {
                return Some(instance);
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
