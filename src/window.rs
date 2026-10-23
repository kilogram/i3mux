//! i3mux windows in the Sway tree.
//!
//! A window is an i3mux window iff it carries a mark
//! `_i3mux:{host}:{terminal}` (plan rule 2). The leading `_` hides the mark
//! in title bars. The same text is the window's `app_id` at launch, so a new
//! window can be found before it is marked.
//!
//! Everything here is pure over `swaymsg -t get_tree` JSON: lookups, and the
//! text of the mark and kill commands. `sway_ops` runs them. This module
//! never imports `wm` (plan rule 8; `just check-boundaries`).

use std::fmt;

use serde_json::Value;

use crate::types::{Host, SessionName, TerminalName};

/// Prefix of every i3mux mark.
const MARK_PREFIX: &str = "_i3mux:";

/// The identity of an i3mux window: which shell, on which host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Mark {
    host: Host,
    terminal: TerminalName,
}

impl Mark {
    #[must_use]
    pub fn new(host: Host, terminal: TerminalName) -> Self {
        Self { host, terminal }
    }

    /// Parse a mark written by `Display`; `None` for any other mark.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (label, terminal) = text.strip_prefix(MARK_PREFIX)?.split_once(':')?;
        Some(Self {
            host: Host::from_label(label).ok()?,
            terminal: terminal.parse().ok()?,
        })
    }

    #[must_use]
    pub fn host(&self) -> &Host {
        &self.host
    }

    #[must_use]
    pub fn terminal(&self) -> &TerminalName {
        &self.terminal
    }

    /// Whether this window shows a shell of `session` on `host`.
    #[must_use]
    pub fn belongs_to(&self, host: &Host, session: &SessionName) -> bool {
        &self.host == host && self.terminal.session() == session
    }

    /// The `app_id` a terminal for this mark is launched with.
    #[must_use]
    pub fn app_id(&self) -> String {
        self.to_string()
    }

    /// `i3mux:{host}:{terminal}`: the title the wrapper gives the window.
    #[must_use]
    pub fn title(&self) -> String {
        format!("i3mux:{}:{}", self.host, self.terminal)
    }
}

impl fmt::Display for Mark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{MARK_PREFIX}{}:{}", self.host, self.terminal)
    }
}

/// Tiled children, then floating children.
pub fn children(node: &Value) -> impl Iterator<Item = &Value> {
    let tiled = node.get("nodes").and_then(Value::as_array);
    let floating = node.get("floating_nodes").and_then(Value::as_array);
    tiled.into_iter().chain(floating).flatten()
}

/// The i3mux mark of `node`, if it has one.
#[must_use]
pub fn node_mark(node: &Value) -> Option<Mark> {
    node.get("marks")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find_map(Mark::parse)
}

fn node_id(node: &Value) -> Option<u64> {
    node.get("id").and_then(Value::as_u64)
}

/// Every node under `node` (inclusive), depth first, tiled before floating.
fn descendants(node: &Value) -> Vec<&Value> {
    let mut out = vec![node];
    for child in children(node) {
        out.extend(descendants(child));
    }
    out
}

/// The workspace called `name`. Workspaces are found by name because every
/// unnumbered workspace has number -1.
#[must_use]
pub fn find_workspace<'a>(tree: &'a Value, name: &str) -> Option<&'a Value> {
    descendants(tree).into_iter().find(|n| {
        n.get("type").and_then(Value::as_str) == Some("workspace")
            && n.get("name").and_then(Value::as_str) == Some(name)
    })
}

/// Container id and mark of every i3mux window under `node`.
#[must_use]
pub fn i3mux_windows_in(node: &Value) -> Vec<(u64, Mark)> {
    descendants(node)
        .into_iter()
        .filter_map(|n| Some((node_id(n)?, node_mark(n)?)))
        .collect()
}

/// Every host that an i3mux window anywhere in the tree is marked with,
/// once each.
#[must_use]
pub fn hosts_with_windows(tree: &Value) -> Vec<Host> {
    let mut hosts = Vec::new();
    for (_, mark) in i3mux_windows_in(tree) {
        if !hosts.contains(&mark.host) {
            hosts.push(mark.host);
        }
    }
    hosts
}

/// Terminals of `session` on `host` that have a window anywhere in the tree.
#[must_use]
pub fn session_terminals(tree: &Value, host: &Host, session: &SessionName) -> Vec<TerminalName> {
    i3mux_windows_in(tree)
        .into_iter()
        .filter(|(_, mark)| mark.belongs_to(host, session))
        .map(|(_, mark)| mark.terminal)
        .collect()
}

/// The container id of the window whose title is `title`: the terminal that
/// set it (PLAN-terminal-here decision 1).
#[must_use]
pub fn find_by_title(tree: &Value, title: &str) -> Option<u64> {
    descendants(tree)
        .into_iter()
        .find(|n| {
            matches!(
                n.get("type").and_then(Value::as_str),
                Some("con" | "floating_con")
            ) && n.get("name").and_then(Value::as_str) == Some(title)
        })
        .and_then(node_id)
}

/// The workspace node that holds the node `con_id`.
#[must_use]
pub fn workspace_containing(tree: &Value, con_id: u64) -> Option<&Value> {
    descendants(tree).into_iter().find(|n| {
        n.get("type").and_then(Value::as_str) == Some("workspace")
            && descendants(n).iter().any(|d| node_id(d) == Some(con_id))
    })
}

/// Every i3mux mark on the node `con_id`.
#[must_use]
pub fn marks_of(tree: &Value, con_id: u64) -> Vec<Mark> {
    descendants(tree)
        .into_iter()
        .find(|n| node_id(n) == Some(con_id))
        .and_then(|n| n.get("marks")?.as_array())
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(Mark::parse)
        .collect()
}

/// The container id of an unmarked window whose `app_id` is `app_id`.
#[must_use]
pub fn find_by_app_id(tree: &Value, app_id: &str) -> Option<u64> {
    descendants(tree)
        .into_iter()
        .find(|n| n.get("app_id").and_then(Value::as_str) == Some(app_id) && node_mark(n).is_none())
        .and_then(node_id)
}

/// `[con_id=N] mark --add "MARK"`: the command that marks window `con_id`.
#[must_use]
pub fn mark_command(con_id: u64, mark: &Mark) -> String {
    format!("[con_id={con_id}] mark --add \"{mark}\"")
}

/// `[con_id=N] unmark "MARK"`: the command that takes `mark` off window
/// `con_id`.
#[must_use]
pub fn unmark_command(con_id: u64, mark: &Mark) -> String {
    format!("[con_id={con_id}] unmark \"{mark}\"")
}

/// `[con_id=N] move container to workspace "NAME"`: the command that moves
/// window `con_id` to the workspace called `name`, which Sway creates if it
/// does not exist. `"` and `\` in the name are escaped.
#[must_use]
pub fn move_to_workspace_command(con_id: u64, name: &str) -> String {
    let quoted = name.replace('\\', "\\\\").replace('"', "\\\"");
    format!("[con_id={con_id}] move container to workspace \"{quoted}\"")
}

/// `[con_mark="^_i3mux:{host}:{session}-[0-9]+$"] kill`: the one command
/// that closes every window of a session. Anchored at both ends so session
/// `ws4` never matches the windows of session `ws4-x`.
#[must_use]
pub fn kill_command(host: &Host, session: &SessionName) -> String {
    format!(
        "[con_mark=\"^{}{}:{}-[0-9]+$\"] kill",
        MARK_PREFIX,
        regex_literal(host.label()),
        regex_literal(session.as_str())
    )
}

/// `text` as a regex that matches only itself. Host and session names hold
/// only alphanumerics, `-`, `_`, `@` and `.`; `.` becomes `[.]`, which needs
/// no backslash inside Sway's quoted criteria.
fn regex_literal(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '@') {
                c.to_string()
            } else {
                format!("[{c}]")
            }
        })
        .collect()
}

#[cfg(test)]
pub mod testing {
    //! A fixture tree from a real `swaymsg -t get_tree` (trimmed, titles
    //! anonymized).
    use serde_json::{json, Value};

    /// Workspaces 1, 2, 3 (a tabbed container of two windows) and `q`
    /// (number -1; one tiled and one floating window), plus a scratchpad
    /// window with a non-i3mux mark.
    pub fn real_tree() -> Value {
        serde_json::from_str(include_str!("testdata/sway-tree.json")).unwrap()
    }

    /// Add `mark` to the node with container id `id`.
    pub fn add_mark(tree: &mut Value, id: u64, mark: &str) {
        let node = find_mut(tree, id).unwrap();
        node["marks"].as_array_mut().unwrap().push(json!(mark));
    }

    fn find_mut(node: &mut Value, id: u64) -> Option<&mut Value> {
        if node["id"].as_u64() == Some(id) {
            return Some(node);
        }
        node.as_object_mut()?
            .iter_mut()
            .filter(|(key, _)| *key == "nodes" || *key == "floating_nodes")
            .filter_map(|(_, kids)| kids.as_array_mut())
            .flat_map(|kids| kids.iter_mut())
            .find_map(|kid| find_mut(kid, id))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{add_mark, real_tree};
    use super::*;
    use crate::types::{strategies, RemoteHost};
    use proptest::prelude::*;

    fn session(name: &str) -> SessionName {
        SessionName::new(name).unwrap()
    }

    fn remote(host: &str) -> Host {
        Host::Ssh(RemoteHost::new(host).unwrap())
    }

    fn mark(host: &Host, session_name: &str, n: u32) -> Mark {
        Mark::new(host.clone(), TerminalName::new(&session(session_name), n))
    }

    /// PLAN-terminal-here decision 1: a window is found by its title, in a
    /// tabbed container or floating too; its workspace is the one that holds
    /// it, and its i3mux marks are all of its marks that parse.
    #[test]
    fn a_window_is_found_by_its_title_and_its_workspace_by_the_window() {
        let mut tree = real_tree();
        assert_eq!(find_by_title(&tree, "firefox window"), Some(263));
        assert_eq!(
            find_by_title(&tree, "md.obsidian.Obsidian window"),
            Some(7),
            "floating"
        );
        assert_eq!(find_by_title(&tree, "no such title"), None);
        assert_eq!(find_by_title(&tree, "q"), None, "a workspace is no window");
        let tabbed = workspace_containing(&tree, 1001).unwrap();
        assert_eq!(tabbed["name"], "3");
        assert_eq!(workspace_containing(&tree, 5).unwrap()["name"], "q");
        assert!(workspace_containing(&tree, 424_242).is_none());
        let m = mark(&Host::Local, "ws3", 2);
        add_mark(&mut tree, 1001, &m.to_string());
        add_mark(&mut tree, 1001, "user-mark");
        assert_eq!(marks_of(&tree, 1001), [m]);
        assert!(marks_of(&tree, 263).is_empty());
    }

    #[test]
    fn mark_format() {
        assert_eq!(
            mark(&Host::Local, "ws1", 1).to_string(),
            "_i3mux:local:ws1-001"
        );
        assert_eq!(
            mark(&remote("dev-box"), "ws1", 1).title(),
            "i3mux:dev-box:ws1-001"
        );
        assert_eq!(
            mark(&remote("user@server.lan"), "ws2", 3).to_string(),
            "_i3mux:user@server.lan:ws2-003"
        );
    }

    #[test]
    fn mark_parse_rejects_other_marks() {
        for text in [
            "random-mark",
            "i3mux:local:ws1-001",
            "_i3mux:nocolon",
            "_i3mux:local:ws1",
            "_i3mux:local:ws1-1",
            "_i3mux:-bad:ws1-001",
            "_i3mux::ws1-001",
            "_i3mux:local:ws 1-001",
        ] {
            assert_eq!(Mark::parse(text), None, "{text}");
        }
    }

    proptest! {
        #[test]
        fn mark_round_trips(host in strategies::host(), terminal in strategies::terminal_name()) {
            // Plan rule 2: the mark is the identity; parse(format(m)) == m.
            let m = Mark::new(host, terminal);
            prop_assert_eq!(Mark::parse(&m.to_string()), Some(m.clone()));
            prop_assert_eq!(Mark::parse(&m.app_id()), Some(m));
        }
    }

    #[test]
    fn kill_selector_matches_only_this_session() {
        let host = remote("user@deep.thought");
        let command = kill_command(&host, &session("ws4"));
        let pattern = command
            .strip_prefix("[con_mark=\"")
            .and_then(|r| r.strip_suffix("\"] kill"))
            .unwrap();
        let re = regex::Regex::new(pattern).unwrap();
        assert!(re.is_match("_i3mux:user@deep.thought:ws4-001"));
        assert!(re.is_match("_i3mux:user@deep.thought:ws4-1234"));
        for other in [
            "_i3mux:user@deep.thought:ws4-x-001",
            "_i3mux:user@deep.thought:ws40-001",
            "_i3mux:user@deepxthought:ws4-001",
            "_i3mux:local:ws4-001",
            "_i3mux:user@deep.thought:ws4-001x",
            "x_i3mux:user@deep.thought:ws4-001",
        ] {
            assert!(!re.is_match(other), "{other}");
        }
    }

    #[test]
    fn regex_literal_quotes_every_allowed_punctuation() {
        assert_eq!(regex_literal("a.b-c_d@e"), "a[.]b-c_d@e");
    }

    #[test]
    fn find_workspace_by_name_including_unnumbered() {
        let tree = real_tree();
        assert_eq!(find_workspace(&tree, "3").unwrap()["id"], 945);
        // Workspace "q" has num -1, like every unnumbered workspace.
        assert_eq!(find_workspace(&tree, "q").unwrap()["num"], -1);
        assert!(find_workspace(&tree, "4").is_none());
    }

    #[test]
    fn real_tree_without_i3mux_marks_has_no_i3mux_windows() {
        // The scratchpad window's mark "scratch-notes" is not an i3mux mark.
        assert!(i3mux_windows_in(&real_tree()).is_empty());
    }

    #[test]
    fn i3mux_windows_include_nested_and_floating() {
        let mut tree = real_tree();
        let a = mark(&Host::Local, "ws3", 1);
        let b = mark(&Host::Local, "ws3", 2);
        let c = mark(&Host::Local, "q", 1);
        add_mark(&mut tree, 1019, &a.to_string());
        add_mark(&mut tree, 1001, &b.to_string());
        add_mark(&mut tree, 5, &c.to_string());
        let ws3 = find_workspace(&tree, "3").unwrap();
        assert_eq!(i3mux_windows_in(ws3), [(1019, a), (1001, b)]);
        let q = find_workspace(&tree, "q").unwrap();
        assert_eq!(i3mux_windows_in(q), [(5, c)]);
    }

    #[test]
    fn session_terminals_filters_by_host_and_session() {
        let mut tree = real_tree();
        add_mark(&mut tree, 1019, &mark(&Host::Local, "ws3", 1).to_string());
        add_mark(&mut tree, 1001, &mark(&Host::Local, "ws3-x", 2).to_string());
        add_mark(&mut tree, 1075, &mark(&remote("h"), "ws3", 3).to_string());
        add_mark(&mut tree, 5, &mark(&Host::Local, "ws3", 4).to_string());
        let found = session_terminals(&tree, &Host::Local, &session("ws3"));
        let numbers: Vec<u32> = found.iter().map(TerminalName::number).collect();
        assert_eq!(numbers, [1, 4]);
    }

    #[test]
    fn find_by_app_id_skips_marked_windows() {
        let mut tree = real_tree();
        assert_eq!(find_by_app_id(&tree, "Alacritty"), Some(1019));
        add_mark(&mut tree, 1019, &mark(&Host::Local, "ws3", 1).to_string());
        assert_eq!(find_by_app_id(&tree, "Alacritty"), Some(1001));
        assert_eq!(find_by_app_id(&tree, "nothing"), None);
    }

    #[test]
    fn move_command_quotes_the_workspace_name() {
        assert_eq!(
            move_to_workspace_command(7, "4"),
            "[con_id=7] move container to workspace \"4\""
        );
        assert_eq!(
            move_to_workspace_command(7, r#"a "b" \c"#),
            r#"[con_id=7] move container to workspace "a \"b\" \\c""#
        );
    }

    #[test]
    fn mark_command_text() {
        let m = mark(&Host::Local, "ws4", 2);
        assert_eq!(
            mark_command(7, &m),
            "[con_id=7] mark --add \"_i3mux:local:ws4-002\""
        );
    }
}
