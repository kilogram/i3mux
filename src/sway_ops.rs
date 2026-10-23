//! Operations on the running Sway through the [`Sway`] port: wait for a
//! launched window and mark it, and close all windows of a session.
//!
//! This is the only module that polls or changes Sway state. The command
//! text and the tree lookups come from `window`, which stays pure over JSON
//! (plan rule 8). `wm::Wm` implements [`Sway`]; tests use `testing::FakeSway`.

use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::types::{Host, SessionName, TerminalName};
use crate::window::{self, Mark};

/// How long to wait for a launched terminal's window.
const WINDOW_WAIT: Duration = Duration::from_secs(5);
const WINDOW_POLL: Duration = Duration::from_millis(100);

/// The Sway IPC operations i3mux needs.
pub trait Sway {
    /// The full layout tree (`swaymsg -t get_tree`).
    ///
    /// # Errors
    /// If Sway cannot be queried.
    fn get_tree(&self) -> Result<Value>;

    /// Run one Sway command.
    ///
    /// # Errors
    /// If Sway rejects the command.
    fn run(&self, command: &str) -> Result<()>;
}

/// Wait for the window launched with `mark.app_id()` and mark it.
/// Returns its container id.
///
/// # Errors
/// If the window does not appear within a few seconds, or Sway fails.
pub fn wait_for_window_and_mark(sway: &dyn Sway, mark: &Mark) -> Result<u64> {
    let app_id = mark.app_id();
    let attempts = WINDOW_WAIT.as_millis() / WINDOW_POLL.as_millis();
    for attempt in 0..attempts {
        if attempt > 0 {
            std::thread::sleep(WINDOW_POLL);
        }
        if let Some(con_id) = window::find_by_app_id(&sway.get_tree()?, &app_id) {
            sway.run(&window::mark_command(con_id, mark))?;
            return Ok(con_id);
        }
    }
    bail!(
        "the window for {} did not appear within {} s; check that $TERMINAL starts",
        mark.terminal(),
        WINDOW_WAIT.as_secs()
    )
}

/// Wait for the window whose title is `title`, which the terminal of this
/// process just set (PLAN-terminal-here decision 1). Returns its container
/// id.
///
/// # Errors
/// If no window takes the title within a few seconds, or Sway fails.
pub fn wait_for_title(sway: &dyn Sway, title: &str) -> Result<u64> {
    let attempts = WINDOW_WAIT.as_millis() / WINDOW_POLL.as_millis();
    for attempt in 0..attempts {
        if attempt > 0 {
            std::thread::sleep(WINDOW_POLL);
        }
        if let Some(con_id) = window::find_by_title(&sway.get_tree()?, title) {
            return Ok(con_id);
        }
    }
    bail!(
        "this terminal's window did not take the title i3mux set within {} s; \
         'i3mux terminal --here' needs a terminal that lets programs set its title",
        WINDOW_WAIT.as_secs()
    )
}

/// Close every window of `session` on `host`, on any workspace, with one
/// Sway command. Returns the terminals whose windows it closed.
///
/// Sway closes the windows asynchronously: their shpool clients may still be
/// attached when this returns.
///
/// # Errors
/// If Sway fails.
pub fn kill_all(sway: &dyn Sway, host: &Host, session: &SessionName) -> Result<Vec<TerminalName>> {
    let closed = window::session_terminals(&sway.get_tree()?, host, session);
    if !closed.is_empty() {
        sway.run(&window::kill_command(host, session))?;
    }
    Ok(closed)
}

#[cfg(test)]
pub mod testing {
    //! An in-memory [`Sway`].
    use super::Sway;
    use crate::window::Mark;
    use anyhow::{anyhow, Result};
    use serde_json::{json, Value};
    use std::sync::Mutex;

    /// An in-memory Sway: one output, workspaces with flat window lists.
    /// Understands `mark --add`, `unmark`, `move container to workspace`
    /// (by `con_id`) and the `con_mark` kill; records every command. As in
    /// Sway, a mark is on one window at most: `mark --add` takes it from
    /// any other window, and adds nothing to a window that has it.
    #[derive(Default)]
    pub struct FakeSway {
        state: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        next_id: u64,
        /// (workspace name, container id, app id, marks)
        windows: Vec<(String, u64, String, Vec<String>)>,
        /// Window titles, by container id.
        titles: std::collections::BTreeMap<u64, String>,
        commands: Vec<String>,
    }

    impl FakeSway {
        /// Open a window with `app_id` on `workspace`; return its id.
        pub fn open(&self, workspace: &str, app_id: &str) -> u64 {
            let mut state = self.state.lock().unwrap();
            state.next_id += 1;
            let id = state.next_id + 100;
            state
                .windows
                .push((workspace.into(), id, app_id.into(), Vec::new()));
            id
        }

        /// Open a window with `app_id` and the title `title` on `workspace`;
        /// return its id.
        pub fn open_titled(&self, workspace: &str, app_id: &str, title: &str) -> u64 {
            let id = self.open(workspace, app_id);
            self.state
                .lock()
                .unwrap()
                .titles
                .insert(id, title.to_owned());
            id
        }

        /// Open an already-marked i3mux window on `workspace`.
        pub fn open_marked(&self, workspace: &str, mark: &Mark) -> u64 {
            let id = self.open(workspace, &mark.app_id());
            self.state
                .lock()
                .unwrap()
                .windows
                .last_mut()
                .unwrap()
                .3
                .push(mark.to_string());
            id
        }

        pub fn commands(&self) -> Vec<String> {
            self.state.lock().unwrap().commands.clone()
        }

        pub fn window_count(&self) -> usize {
            self.state.lock().unwrap().windows.len()
        }
    }

    impl Sway for FakeSway {
        fn get_tree(&self) -> Result<Value> {
            let state = self.state.lock().unwrap();
            let mut names: Vec<&String> = state.windows.iter().map(|w| &w.0).collect();
            names.sort();
            names.dedup();
            let workspaces: Vec<Value> = names
                .into_iter()
                .map(|ws| {
                    let windows: Vec<Value> = state
                        .windows
                        .iter()
                        .filter(|w| &w.0 == ws)
                        .map(|(_, id, app_id, marks)| {
                            json!({"id": id, "type": "con", "layout": "none",
                                   "app_id": app_id, "marks": marks,
                                   "name": state.titles.get(id),
                                   "nodes": [], "floating_nodes": []})
                        })
                        .collect();
                    let num = ws.parse::<i32>().unwrap_or(-1);
                    json!({"id": 50, "type": "workspace", "name": ws, "num": num,
                           "focused": false, "layout": "splith",
                           "marks": [], "nodes": windows, "floating_nodes": []})
                })
                .collect();
            Ok(
                json!({"id": 1, "type": "root", "marks": [], "floating_nodes": [], "nodes": [
                    {"id": 2, "type": "output", "marks": [], "floating_nodes": [], "nodes": workspaces}
                ]}),
            )
        }

        fn run(&self, command: &str) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            state.commands.push(command.to_owned());
            if let Some(rest) = command.strip_prefix("[con_id=") {
                let (id, rest) = rest.split_once(']').ok_or_else(|| anyhow!("bad command"))?;
                let id: u64 = id.parse()?;
                let rest = rest.trim();
                let added = rest
                    .strip_prefix("mark --add ")
                    .map(|mark| mark.trim_matches('"').to_owned());
                if let Some(mark) = &added {
                    for other in state.windows.iter_mut().filter(|w| w.1 != id) {
                        other.3.retain(|m| m != mark);
                    }
                }
                let window = state
                    .windows
                    .iter_mut()
                    .find(|w| w.1 == id)
                    .ok_or_else(|| anyhow!("no {id}"))?;
                if let Some(mark) = added {
                    if !window.3.contains(&mark) {
                        window.3.push(mark);
                    }
                } else if let Some(mark) = rest.strip_prefix("unmark ") {
                    let mark = mark.trim_matches('"');
                    window.3.retain(|m| m != mark);
                } else if let Some(name) = rest.strip_prefix("move container to workspace ") {
                    let name = name.strip_prefix('"').and_then(|n| n.strip_suffix('"'));
                    let name = name.ok_or_else(|| anyhow!("unquoted workspace"))?;
                    window.0 = name.replace("\\\"", "\"").replace("\\\\", "\\");
                } else {
                    return Err(anyhow!("bad command: {command}"));
                }
            } else if let Some(rest) = command.strip_prefix("[con_mark=\"") {
                let (pattern, _) = rest.split_once("\"]").ok_or_else(|| anyhow!("bad"))?;
                let re = regex::Regex::new(pattern)?;
                state
                    .windows
                    .retain(|w| !w.3.iter().any(|m| re.is_match(m)));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeSway;
    use super::*;

    fn session(name: &str) -> SessionName {
        SessionName::new(name).unwrap()
    }

    fn mark(session_name: &str, n: u32) -> Mark {
        Mark::new(Host::Local, TerminalName::new(&session(session_name), n))
    }

    #[test]
    fn wait_for_window_marks_the_new_window() {
        let sway = FakeSway::default();
        let m = mark("ws4", 2);
        let id = sway.open("4", &m.app_id());
        assert_eq!(wait_for_window_and_mark(&sway, &m).unwrap(), id);
        assert_eq!(sway.commands(), [window::mark_command(id, &m)]);
        let tree = sway.get_tree().unwrap();
        assert_eq!(
            window::session_terminals(&tree, &Host::Local, &session("ws4")),
            [m.terminal().clone()]
        );
    }

    #[test]
    fn kill_all_closes_only_this_session_in_one_command() {
        let sway = FakeSway::default();
        sway.open_marked("4", &mark("ws4", 1));
        sway.open_marked("5", &mark("ws4", 2));
        sway.open_marked("4", &mark("ws4-x", 1));
        sway.open("4", "firefox");
        let closed = kill_all(&sway, &Host::Local, &session("ws4")).unwrap();
        assert_eq!(
            closed,
            [
                mark("ws4", 1).terminal().clone(),
                mark("ws4", 2).terminal().clone()
            ]
        );
        assert_eq!(sway.commands().len(), 1);
        assert_eq!(sway.window_count(), 2);
    }

    #[test]
    fn kill_all_without_windows_sends_nothing() {
        let sway = FakeSway::default();
        assert_eq!(kill_all(&sway, &Host::Local, &session("ws4")).unwrap(), []);
        assert!(sway.commands().is_empty());
    }
}
