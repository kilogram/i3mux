//! The layout of a workspace's i3mux windows: capture from the Sway tree,
//! and the command sequence that rebuilds it.
//!
//! Both directions are pure. [`capture`] reads `get_tree` JSON;
//! [`restore_plan`] returns [`Step`]s for the caller to run. This module
//! never imports `wm` (plan rule 8).
//!
//! The serde format is the saved-session format; keep it stable.
//! [`restore_plan`] reproduces the command sequence that the layout golden
//! images were made with; change it only together with the goldens.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{Host, SessionName, TerminalName};
use crate::window::{children, node_mark};

/// A tree of containers whose leaves are i3mux terminals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Layout {
    #[serde(rename = "hsplit")]
    HSplit {
        children: Vec<Layout>,
        #[serde(skip_serializing_if = "Option::is_none")]
        percent: Option<f64>,
    },
    #[serde(rename = "vsplit")]
    VSplit {
        children: Vec<Layout>,
        #[serde(skip_serializing_if = "Option::is_none")]
        percent: Option<f64>,
    },
    #[serde(rename = "tabbed")]
    Tabbed { children: Vec<Layout> },
    #[serde(rename = "stacked")]
    Stacked { children: Vec<Layout> },
    #[serde(rename = "terminal")]
    Terminal {
        #[serde(rename = "socket")]
        terminal: TerminalName,
        #[serde(skip_serializing_if = "Option::is_none")]
        percent: Option<f64>,
    },
}

impl Layout {
    /// The terminals, leftmost/topmost first.
    #[must_use]
    pub fn terminals(&self) -> Vec<&TerminalName> {
        match self {
            Self::Terminal { terminal, .. } => vec![terminal],
            Self::HSplit { children, .. }
            | Self::VSplit { children, .. }
            | Self::Tabbed { children }
            | Self::Stacked { children } => children.iter().flat_map(Self::terminals).collect(),
        }
    }

    fn is_container(&self) -> bool {
        !matches!(self, Self::Terminal { .. })
    }
}

/// The layout of the windows of `session` on `host` inside `workspace`, or
/// `None` if it has none. Floating windows become tiled children of their
/// parent. Windows of other sessions are left out.
#[must_use]
pub fn capture(workspace: &Value, host: &Host, session: &SessionName) -> Option<Layout> {
    let percent = workspace.get("percent").and_then(Value::as_f64);
    if let Some(mark) = node_mark(workspace).filter(|m| m.belongs_to(host, session)) {
        return Some(Layout::Terminal {
            terminal: mark.terminal().clone(),
            percent,
        });
    }
    let children: Vec<Layout> = children(workspace)
        .filter_map(|child| capture(child, host, session))
        .collect();
    if children.is_empty() {
        return None;
    }
    Some(match workspace.get("layout").and_then(Value::as_str) {
        Some("splith") => Layout::HSplit { children, percent },
        Some("tabbed") => Layout::Tabbed { children },
        Some("stacked") => Layout::Stacked { children },
        _ => Layout::VSplit { children, percent },
    })
}

/// One step of rebuilding a layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Open the window for this terminal and wait until it is marked.
    Launch(TerminalName),
    /// Run this Sway command.
    Sway(&'static str),
}

/// The steps that rebuild `layout` in an empty, focused workspace.
///
/// Each container opens its first child, sets the split or layout once, then
/// opens the remaining children, which join it as siblings. A tabbed or
/// stacked container whose first child is itself a container first moves
/// focus up past that child, and afterwards returns focus to the first tab.
#[must_use]
pub fn restore_plan(layout: &Layout) -> Vec<Step> {
    let mut steps = Vec::new();
    plan_into(layout, &mut steps);
    steps
}

fn plan_into(layout: &Layout, steps: &mut Vec<Step>) {
    match layout {
        Layout::Terminal { terminal, .. } => steps.push(Step::Launch(terminal.clone())),
        Layout::HSplit { children, .. } => plan_split(children, "split h", steps),
        Layout::VSplit { children, .. } => plan_split(children, "split v", steps),
        Layout::Tabbed { children } => plan_grouped(children, "layout tabbed", "focus left", steps),
        Layout::Stacked { children } => {
            plan_grouped(children, "layout stacking", "focus up", steps);
        }
    }
}

fn plan_split(children: &[Layout], split: &'static str, steps: &mut Vec<Step>) {
    let Some((first, rest)) = children.split_first() else {
        return;
    };
    plan_into(first, steps);
    if rest.is_empty() {
        return;
    }
    steps.push(Step::Sway(split));
    for child in rest {
        plan_into(child, steps);
    }
}

fn plan_grouped(
    children: &[Layout],
    layout: &'static str,
    back_to_first: &'static str,
    steps: &mut Vec<Step>,
) {
    let Some((first, rest)) = children.split_first() else {
        return;
    };
    plan_into(first, steps);
    if rest.is_empty() {
        return;
    }
    let nested = first.is_container();
    if nested {
        steps.extend([Step::Sway("focus parent"), Step::Sway("focus parent")]);
    }
    steps.push(Step::Sway(layout));
    if nested {
        steps.push(Step::Sway("focus parent"));
    }
    for child in rest {
        plan_into(child, steps);
    }
    if nested {
        steps.extend([
            Step::Sway("focus parent"),
            Step::Sway("focus parent"),
            Step::Sway(back_to_first),
        ]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::testing::{add_mark, real_tree};
    use crate::window::{find_workspace, Mark};
    use serde_json::json;

    fn ws4() -> SessionName {
        SessionName::new("ws4").unwrap()
    }

    fn t(n: u32) -> TerminalName {
        TerminalName::new(&ws4(), n)
    }

    fn term(n: u32) -> Layout {
        Layout::Terminal {
            terminal: t(n),
            percent: None,
        }
    }

    fn hsplit(children: Vec<Layout>) -> Layout {
        Layout::HSplit {
            children,
            percent: None,
        }
    }

    fn vsplit(children: Vec<Layout>) -> Layout {
        Layout::VSplit {
            children,
            percent: None,
        }
    }

    fn tabbed(children: Vec<Layout>) -> Layout {
        Layout::Tabbed { children }
    }

    fn stacked(children: Vec<Layout>) -> Layout {
        Layout::Stacked { children }
    }

    // ----- Sway-shaped JSON fixtures (keys as in `swaymsg -t get_tree`) -----

    fn node(kind: &str, layout: &str, nodes: &[Value]) -> Value {
        json!({"id": 10, "type": kind, "layout": layout, "percent": null, "marks": [],
               "nodes": nodes, "floating_nodes": []})
    }

    fn workspace(layout: &str, nodes: &[Value]) -> Value {
        let mut ws = node("workspace", layout, nodes);
        ws["name"] = json!("4");
        ws["num"] = json!(4);
        ws
    }

    fn con(layout: &str, nodes: &[Value]) -> Value {
        node("con", layout, nodes)
    }

    fn window(n: u32) -> Value {
        let mark = Mark::new(Host::Local, t(n));
        let mut w = node("con", "none", &[]);
        w["id"] = json!(1000 + n);
        w["app_id"] = json!(mark.app_id());
        w["marks"] = json!([mark.to_string()]);
        w
    }

    fn capture_ws4(ws: &Value) -> Option<Layout> {
        capture(ws, &Host::Local, &ws4())
    }

    // ----- capture -----

    #[test]
    fn capture_hsplit_2() {
        let ws = workspace("splith", &[window(1), window(2)]);
        assert_eq!(capture_ws4(&ws), Some(hsplit(vec![term(1), term(2)])));
    }

    #[test]
    fn capture_tabbed_3() {
        let ws = workspace(
            "splith",
            &[con("tabbed", &[window(1), window(2), window(3)])],
        );
        assert_eq!(
            capture_ws4(&ws),
            Some(hsplit(vec![tabbed(vec![term(1), term(2), term(3)])]))
        );
    }

    #[test]
    fn capture_tabs_in_hsplit() {
        let ws = workspace(
            "splith",
            &[window(1), con("tabbed", &[window(2), window(3)])],
        );
        assert_eq!(
            capture_ws4(&ws),
            Some(hsplit(vec![term(1), tabbed(vec![term(2), term(3)])]))
        );
    }

    #[test]
    fn capture_hsplit_in_tabs() {
        let ws = workspace(
            "splith",
            &[con(
                "tabbed",
                &[con("splith", &[window(1), window(2)]), window(3)],
            )],
        );
        assert_eq!(
            capture_ws4(&ws),
            Some(hsplit(vec![tabbed(vec![
                hsplit(vec![term(1), term(2)]),
                term(3)
            ])]))
        );
    }

    #[test]
    fn capture_nested() {
        let ws = workspace(
            "splith",
            &[
                window(1),
                con(
                    "splitv",
                    &[window(2), con("stacked", &[window(3), window(4)])],
                ),
            ],
        );
        assert_eq!(
            capture_ws4(&ws),
            Some(hsplit(vec![
                term(1),
                vsplit(vec![term(2), stacked(vec![term(3), term(4)])])
            ]))
        );
    }

    #[test]
    fn capture_ignores_other_windows_and_sessions() {
        let mut foreign = window(9);
        foreign["marks"] = json!(["_i3mux:local:ws4-x-009"]);
        let mut remote = window(8);
        remote["marks"] = json!(["_i3mux:deepthought:ws4-008"]);
        let mut plain = node("con", "none", &[]);
        plain["app_id"] = json!("firefox");
        let ws = workspace(
            "splith",
            &[window(1), foreign, con("splitv", &[plain, remote])],
        );
        assert_eq!(capture_ws4(&ws), Some(hsplit(vec![term(1)])));
    }

    #[test]
    fn capture_of_workspace_without_i3mux_windows_is_none() {
        let tree = real_tree();
        assert_eq!(capture_ws4(find_workspace(&tree, "3").unwrap()), None);
    }

    #[test]
    fn capture_from_real_tree_with_tabbed_container() {
        // Workspace 3 on a real Sway: splith workspace, tabbed con, two windows.
        let mut tree = real_tree();
        add_mark(&mut tree, 1019, "_i3mux:local:ws4-001");
        add_mark(&mut tree, 1001, "_i3mux:local:ws4-002");
        let ws = find_workspace(&tree, "3").unwrap();
        let percent = Some(1.0);
        assert_eq!(
            capture_ws4(ws),
            Some(hsplit(vec![Layout::Tabbed {
                children: vec![
                    Layout::Terminal {
                        terminal: t(1),
                        percent
                    },
                    Layout::Terminal {
                        terminal: t(2),
                        percent
                    },
                ]
            }]))
        );
    }

    #[test]
    fn capture_turns_floating_windows_into_children() {
        let mut tree = real_tree();
        add_mark(&mut tree, 1075, "_i3mux:local:ws4-001");
        add_mark(&mut tree, 5, "_i3mux:local:ws4-002");
        let layout = capture_ws4(find_workspace(&tree, "q").unwrap()).unwrap();
        assert_eq!(layout.terminals(), [&t(1), &t(2)]);
        assert!(matches!(layout, Layout::HSplit { .. }));
    }

    // ----- restore plan (sequence verified by the layout goldens) -----

    fn launch(n: u32) -> Step {
        Step::Launch(t(n))
    }

    fn sway(command: &'static str) -> Step {
        Step::Sway(command)
    }

    #[test]
    fn plan_hsplit_2() {
        assert_eq!(
            restore_plan(&hsplit(vec![term(1), term(2)])),
            [launch(1), sway("split h"), launch(2)]
        );
    }

    #[test]
    fn plan_tabbed_3() {
        assert_eq!(
            restore_plan(&hsplit(vec![tabbed(vec![term(1), term(2), term(3)])])),
            [launch(1), sway("layout tabbed"), launch(2), launch(3)]
        );
    }

    #[test]
    fn plan_tabs_in_hsplit() {
        assert_eq!(
            restore_plan(&hsplit(vec![term(1), tabbed(vec![term(2), term(3)])])),
            [
                launch(1),
                sway("split h"),
                launch(2),
                sway("layout tabbed"),
                launch(3)
            ]
        );
    }

    #[test]
    fn plan_hsplit_in_tabs() {
        assert_eq!(
            restore_plan(&hsplit(vec![tabbed(vec![
                hsplit(vec![term(1), term(2)]),
                term(3)
            ])])),
            [
                launch(1),
                sway("split h"),
                launch(2),
                sway("focus parent"),
                sway("focus parent"),
                sway("layout tabbed"),
                sway("focus parent"),
                launch(3),
                sway("focus parent"),
                sway("focus parent"),
                sway("focus left"),
            ]
        );
    }

    #[test]
    fn plan_nested() {
        assert_eq!(
            restore_plan(&hsplit(vec![
                term(1),
                vsplit(vec![term(2), stacked(vec![term(3), term(4)])])
            ])),
            [
                launch(1),
                sway("split h"),
                launch(2),
                sway("split v"),
                launch(3),
                sway("layout stacking"),
                launch(4),
            ]
        );
    }

    #[test]
    fn plan_stacked_with_container_first_returns_focus_up() {
        assert_eq!(
            restore_plan(&stacked(vec![vsplit(vec![term(1), term(2)]), term(3)])),
            [
                launch(1),
                sway("split v"),
                launch(2),
                sway("focus parent"),
                sway("focus parent"),
                sway("layout stacking"),
                sway("focus parent"),
                launch(3),
                sway("focus parent"),
                sway("focus parent"),
                sway("focus up"),
            ]
        );
    }

    #[test]
    fn plan_launches_every_terminal_once_in_order() {
        let layout = hsplit(vec![
            term(1),
            tabbed(vec![vsplit(vec![term(2), term(3)]), term(4)]),
        ]);
        let launched: Vec<TerminalName> = restore_plan(&layout)
            .into_iter()
            .filter_map(|step| match step {
                Step::Launch(terminal) => Some(terminal),
                Step::Sway(_) => None,
            })
            .collect();
        let expected: Vec<TerminalName> = layout.terminals().into_iter().cloned().collect();
        assert_eq!(launched, expected);
    }

    #[test]
    fn empty_containers_plan_nothing() {
        assert_eq!(restore_plan(&hsplit(vec![])), []);
        assert_eq!(restore_plan(&tabbed(vec![])), []);
    }

    // ----- serde format -----

    #[test]
    fn serde_format_is_stable() {
        let layout = hsplit(vec![
            Layout::Terminal {
                terminal: t(1),
                percent: Some(0.5),
            },
            tabbed(vec![term(2)]),
        ]);
        let json = serde_json::to_value(&layout).unwrap();
        assert_eq!(
            json,
            json!({"type": "hsplit", "children": [
                {"type": "terminal", "socket": "ws4-001", "percent": 0.5},
                {"type": "tabbed", "children": [{"type": "terminal", "socket": "ws4-002"}]}
            ]})
        );
        assert_eq!(serde_json::from_value::<Layout>(json).unwrap(), layout);
    }

    #[test]
    fn deserialize_rejects_invalid_terminal_names() {
        let bad = json!({"type": "terminal", "socket": "not a terminal"});
        assert!(serde_json::from_value::<Layout>(bad).is_err());
    }
}
