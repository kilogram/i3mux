//! One test's world: fresh containers, the i3mux CLI, Sway queries, the
//! shells on each host, screenshots and golden images.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use image::RgbaImage;
use serde_json::Value;

use super::comparison_spec::ComparisonSpec;
use super::docker::Containers;
use super::i3mux::{I3mux, Session};
use super::screenshot::{compare_screenshots, load_golden_image, save_comparison_failure};

/// How long to wait for Sway, shpool or ssh to reach an expected state.
pub const WAIT: Duration = Duration::from_secs(15);

/// Golden images live in `tests/integration/golden/sway/`.
const GOLDEN_DIR: &str = "sway";

/// A shell as `shpool list --json` reports it on its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostShell {
    pub name: String,
    pub attached: bool,
    pub pid: u64,
}

/// Fresh containers for one test. Dropping it removes them.
pub struct TestEnvironment {
    containers: Containers,
    update_goldens: bool,
}

impl TestEnvironment {
    pub fn new() -> Result<Self> {
        let containers = Containers::start().context("start the test containers")?;
        let update_goldens = std::env::var_os("UPDATE_GOLDENS").is_some();
        Ok(Self {
            containers,
            update_goldens,
        })
    }

    #[must_use]
    pub fn i3mux(&self) -> I3mux<'_> {
        I3mux::new(&self.containers)
    }

    // ------------------------------------------------------------ sway

    /// Run one Sway command.
    pub fn sway(&self, command: &str) -> Result<()> {
        let output = self.containers.exec_in_sway(&format!(
            "source /tmp/sway-env.sh && swaymsg -- {}",
            shell_words::quote(command)
        ))?;
        if !output.status.success() {
            bail!(
                "swaymsg {command:?} failed: {}{}",
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Focus (and create) workspace `number`.
    pub fn focus_workspace(&self, number: u32) -> Result<()> {
        self.sway(&format!("workspace {number}"))
    }

    /// Focus workspace `number`, then `i3mux activate` it for `session`.
    pub fn activate(&self, session: Session, number: u32) -> Result<()> {
        self.focus_workspace(number)?;
        self.i3mux().activate(session)?;
        Ok(())
    }

    fn tree(&self) -> Result<Value> {
        let output = self
            .containers
            .exec_in_sway("source /tmp/sway-env.sh && swaymsg -t get_tree")?;
        if !output.status.success() {
            bail!("swaymsg -t get_tree failed");
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    /// The windows (views) of workspace `number`, as `(app_id, marks)`.
    pub fn windows(&self, number: u32) -> Result<Vec<(String, Vec<String>)>> {
        let tree = self.tree()?;
        let name = number.to_string();
        let mut out = Vec::new();
        if let Some(workspace) = find_node(&tree, &|n| {
            n["type"] == "workspace" && n["name"].as_str() == Some(name.as_str())
        }) {
            collect_views(workspace, &mut out);
        }
        Ok(out)
    }

    /// Wait until workspace `number` has `count` windows. Sway closes
    /// windows asynchronously, and `detach` and `kill` do not wait for it.
    pub fn wait_for_window_count(&self, number: u32, count: usize) -> Result<()> {
        wait_until(&format!("workspace {number} has {count} windows"), || {
            Ok(self.windows(number)?.len() == count)
        })
    }

    /// The i3mux marks on workspace `number`, sorted.
    pub fn marks(&self, number: u32) -> Result<Vec<String>> {
        let mut marks: Vec<String> = self
            .windows(number)?
            .into_iter()
            .flat_map(|(_, marks)| marks)
            .filter(|m| m.starts_with("_i3mux:"))
            .collect();
        marks.sort();
        Ok(marks)
    }

    /// Close the window with `mark` the way a user does.
    pub fn close_window(&self, mark: &str) -> Result<()> {
        self.sway(&format!("[con_mark=\"^{mark}$\"] kill"))
    }

    /// Start a plain (non-i3mux) foot window on the focused workspace.
    pub fn launch_plain_terminal(&self, number: u32, command: &str) -> Result<()> {
        let before = self.windows(number)?.len();
        self.sway(&format!("exec env LANG=C.UTF-8 foot {command}"))?;
        wait_until("the plain terminal appears", || {
            Ok(self.windows(number)?.len() > before)
        })
    }

    // ------------------------------------------------------------ hosts

    /// The shells of the i3mux shpool daemon on `session`'s host.
    pub fn shells(&self, session: Session) -> Result<Vec<HostShell>> {
        let cmd = format!(
            "shpool --socket {}/.local/run/i3mux/shpool.socket list --json",
            session.home()
        );
        let output = self.exec_on_host(session, &cmd)?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            if stderr.contains("could not connect to daemon") {
                return Ok(Vec::new());
            }
            bail!("shpool list failed: {stderr}");
        }
        let json: Value = serde_json::from_slice(&output.stdout)?;
        let mut shells: Vec<HostShell> = json["sessions"]
            .as_array()
            .context("shpool list --json has no sessions array")?
            .iter()
            .map(|s| HostShell {
                name: s["name"].as_str().unwrap_or_default().to_owned(),
                attached: s["status"] != "Disconnected",
                pid: s["shell_pid"].as_u64().unwrap_or_default(),
            })
            .collect();
        shells.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(shells)
    }

    /// End shell `name` on `session`'s host, as a reboot would.
    pub fn kill_host_shell(&self, session: Session, name: &str) -> Result<()> {
        let cmd = format!(
            "shpool --socket {}/.local/run/i3mux/shpool.socket kill {name}",
            session.home()
        );
        let output = self.exec_on_host(session, &cmd)?;
        if !output.status.success() {
            bail!(
                "shpool kill {name} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    /// The shell `name` on `session`'s host, if it is alive.
    pub fn shell(&self, session: Session, name: &str) -> Result<Option<HostShell>> {
        Ok(self.shells(session)?.into_iter().find(|s| s.name == name))
    }

    /// Wait until no shell on `session`'s host has a client. A window closed
    /// by hand keeps its client for a moment (`i3mux detach` waits for its
    /// own windows itself).
    pub fn wait_until_all_disconnected(&self, session: Session) -> Result<()> {
        wait_until("every shell is disconnected", || {
            Ok(self.shells(session)?.iter().all(|s| !s.attached))
        })
    }

    /// The content of `~/{path}` on `session`'s host, if it exists.
    pub fn host_file(&self, session: Session, path: &str) -> Result<Option<String>> {
        let full = format!("{}/{path}", session.home());
        let output =
            self.exec_on_host(session, &format!("cat -- {}", shell_words::quote(&full)))?;
        Ok(output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
    }

    /// Write `content` to `~/{path}` on `session`'s host.
    pub fn write_host_file(&self, session: Session, path: &str, content: &str) -> Result<()> {
        let full = format!("{}/{path}", session.home());
        let cmd = format!(
            "printf '%s' {} > {}",
            shell_words::quote(content),
            shell_words::quote(&full)
        );
        let output = self.exec_on_host(session, &cmd)?;
        if !output.status.success() {
            bail!("cannot write {full}");
        }
        Ok(())
    }

    /// Delete `~/{path}` on `session`'s host.
    pub fn remove_host_file(&self, session: Session, path: &str) -> Result<()> {
        let full = format!("{}/{path}", session.home());
        let output = self.exec_on_host(session, &format!("rm -- {}", shell_words::quote(&full)))?;
        if !output.status.success() {
            bail!("cannot remove {full}");
        }
        Ok(())
    }

    /// Attach shell `name` from a second client on its host, in a pty of
    /// 24x80 (shpool's client exits at once on a 0x0 pty), and wait until
    /// shpool reports it attached.
    pub fn attach_second_client(&self, session: Session, name: &str) -> Result<()> {
        let attach = format!(
            "stty rows 24 cols 80; exec shpool --socket {}/.local/run/i3mux/shpool.socket attach {name}",
            session.home()
        );
        // script's stdin must stay open: at EOF it hangs up the shell.
        let cmd = format!(
            "sleep 100000 | TERM=xterm-256color script -qfc {} /dev/null > /tmp/second-client.log 2>&1",
            shell_words::quote(&attach)
        );
        self.containers
            .spawn_detached(session == Session::Remote, &cmd)?;
        wait_until("the second client is attached", || {
            Ok(self.shell(session, name)?.is_some_and(|s| s.attached))
        })
    }

    /// Whether the second client started by [`Self::attach_second_client`]
    /// still runs.
    pub fn second_client_running(&self, session: Session) -> Result<bool> {
        let output = self.exec_on_host(session, "pgrep -f '^script -qfc'")?;
        Ok(output.status.success())
    }

    fn exec_on_host(&self, session: Session, cmd: &str) -> Result<std::process::Output> {
        match session {
            Session::Local => self.containers.exec_in_sway(cmd),
            Session::Remote => self.containers.exec_in_remote(cmd),
        }
    }

    // ------------------------------------------------------------ waiting

    // ------------------------------------------------------------ spec actions

    /// Run one spec action (`msg ...`, `launch_terminal [COLOR]`) in the
    /// Sway container. Both scripts are in `tests/docker/scripts`.
    pub fn exec_action(&self, action: &str) -> Result<()> {
        let output = self.containers.exec_in_sway(&format!(
            "source /tmp/sway-env.sh && export TERMINAL=foot LANG=C.UTF-8 && {action}"
        ))?;
        if !output.status.success() {
            bail!(
                "action {action:?} failed:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        thread::sleep(Duration::from_millis(200));
        Ok(())
    }

    pub fn exec_actions(&self, actions: &[String]) -> Result<()> {
        actions.iter().try_for_each(|a| self.exec_action(a))
    }

    /// `launch_terminal COLOR`: an i3mux terminal filled with one color.
    pub fn launch_color(&self, color: &str) -> Result<()> {
        self.exec_action(&format!("launch_terminal {color}"))
    }

    // ------------------------------------------------------------ screenshots

    fn capture(&self) -> Result<RgbaImage> {
        let path = "/tmp/screenshots/shot.png";
        let output = self.containers.exec_in_sway(&format!(
            "mkdir -p /tmp/screenshots && source /tmp/sway-env.sh && grim {path}"
        ))?;
        if !output.status.success() {
            bail!("grim failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        let dir = tempfile::tempdir()?;
        let local = dir.path().join("shot.png");
        self.containers.copy_from_sway(path, &local)?;
        Ok(image::open(&local)
            .context("cannot decode the screenshot")?
            .to_rgba8())
    }

    /// A screenshot taken once two captures in a row are identical, so
    /// terminals that are still drawing (ssh, replay) do not race the test.
    pub fn capture_screenshot(&self) -> Result<RgbaImage> {
        let deadline = Instant::now() + WAIT;
        let mut previous = self.capture()?;
        loop {
            thread::sleep(Duration::from_millis(400));
            let current = self.capture()?;
            if current == previous || Instant::now() > deadline {
                return Ok(current);
            }
            previous = current;
        }
    }

    /// Focus each of `count` tabs or stack entries in turn with
    /// `focus {direction}` and capture each.
    pub fn capture_each(&self, count: usize, direction: &str) -> Result<Vec<RgbaImage>> {
        let mut shots = Vec::with_capacity(count);
        for i in 0..count {
            if i > 0 {
                self.sway(&format!("focus {direction}"))?;
            }
            shots.push(self.capture_screenshot()?);
        }
        Ok(shots)
    }

    /// Compare `actual` with golden `name`, or replace the golden when
    /// `UPDATE_GOLDENS` is set.
    pub fn compare_with_golden(
        &self,
        name: &str,
        actual: &RgbaImage,
        spec: &ComparisonSpec,
    ) -> Result<()> {
        let subpath = format!("{GOLDEN_DIR}/{name}");
        if self.update_goldens {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/integration/golden")
                .join(&subpath);
            actual.save(&path)?;
            println!("  updated golden {subpath}");
            return Ok(());
        }
        let golden = load_golden_image(&subpath)?;
        let result = compare_screenshots(&golden, actual, spec)?;
        if !result.passed {
            let test = thread::current().name().unwrap_or("unknown").to_owned();
            let dir = save_comparison_failure(&test, &golden, actual, &result)?;
            bail!(
                "screenshot differs from golden {subpath}: {} pixels ({:.2}%); artifacts in {}",
                result.total_diff_pixels,
                result.diff_percentage,
                dir.display()
            );
        }
        Ok(())
    }

    /// Compare shots with goldens `{base}-1.png`, `{base}-2.png`, ...
    pub fn compare_each_with_golden(
        &self,
        base: &str,
        shots: &[RgbaImage],
        spec: &ComparisonSpec,
    ) -> Result<()> {
        for (i, shot) in shots.iter().enumerate() {
            self.compare_with_golden(&format!("{base}-{}.png", i + 1), shot, spec)?;
        }
        Ok(())
    }
}

/// Poll `check` until it holds, for up to [`WAIT`].
pub fn wait_until(what: &str, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        if check()? {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("timed out after {WAIT:?} waiting until {what}");
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Compare two screenshots of one run (no golden involved), with the
/// thresholds of `spec`. Saves the usual artifacts on failure.
pub fn assert_same_screen(
    expected: &RgbaImage,
    actual: &RgbaImage,
    spec: &ComparisonSpec,
) -> Result<()> {
    let result = compare_screenshots(expected, actual, spec)?;
    if !result.passed {
        let test = thread::current().name().unwrap_or("unknown").to_owned();
        let dir = save_comparison_failure(&test, expected, actual, &result)?;
        bail!(
            "screen differs from the earlier capture: {} pixels ({:.2}%); artifacts in {}",
            result.total_diff_pixels,
            result.diff_percentage,
            dir.display()
        );
    }
    Ok(())
}

fn find_node<'t>(node: &'t Value, pred: &dyn Fn(&Value) -> bool) -> Option<&'t Value> {
    if pred(node) {
        return Some(node);
    }
    ["nodes", "floating_nodes"]
        .iter()
        .filter_map(|key| node[*key].as_array())
        .flatten()
        .find_map(|child| find_node(child, pred))
}

fn collect_views(node: &Value, out: &mut Vec<(String, Vec<String>)>) {
    if let Some(app_id) = node["app_id"].as_str() {
        let marks = node["marks"]
            .as_array()
            .map(|m| {
                m.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        out.push((app_id.to_owned(), marks));
    }
    for key in ["nodes", "floating_nodes"] {
        for child in node[key].as_array().into_iter().flatten() {
            collect_views(child, out);
        }
    }
}
