//! Launch terminal emulator windows.
//!
//! An i3mux window runs the wrapper in `$TERMINAL`:
//!
//! ```text
//! wrapper.sh TITLE ARGV...
//! wrapper.sh --remote HOST SHELL UNIT N REATTACH... TITLE ARGV...
//! ```
//!
//! The wrapper sets the title once, runs ARGV with no shell, and keeps the
//! window open if ARGV fails (plan decisions 10 and 13). A remote window
//! also recovers from a lost ssh connection: it shows a landing page while
//! UNIT, the host's ssh master, reconnects, then runs REATTACH, the N-word
//! argv that attaches SHELL with `--force` (plan-ssh-recovery rules 7-11).
//! [`wrapper_argv`] is the only code that builds this command line. Every
//! word reaches the wrapper as one argument, so nothing is quoted.
//!
//! Spawned terminals get no stdin, stdout or stderr from i3mux and run in
//! their own process group. A caller that captures i3mux's output (the rofi
//! script does) therefore never waits for the terminals to exit.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::master::UnitName;
use crate::types::{RemoteHost, TerminalName};

/// The wrapper script, written to the private runtime directory.
pub const WRAPPER: &str = include_str!("wrapper.sh");
const WRAPPER_FILE: &str = "wrapper.sh";

/// What an i3mux window runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowCommand {
    /// Run the argv: a local shell, or a remote one on a machine that runs
    /// no ssh masters.
    Plain(Vec<String>),
    /// Attach a remote shell through its host's ssh master, and recover when
    /// the connection drops.
    Remote(RemoteAttach),
}

impl WindowCommand {
    /// The argv the window runs first.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        match self {
            Self::Plain(argv) => argv,
            Self::Remote(remote) => &remote.argv,
        }
    }
}

/// A remote window, and what it needs to recover from a lost connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAttach {
    /// The attach argv.
    pub argv: Vec<String>,
    /// The attach argv with `--force`, run once the connection is back: the
    /// host may still count the lost client as attached.
    pub reattach: Vec<String>,
    /// The ssh master unit of `host`, which the window watches.
    pub unit: UnitName,
    pub host: RemoteHost,
    pub shell: TerminalName,
}

/// Opens terminal windows. [`TerminalLauncher`] is the real one; tests use
/// fakes that add windows to a fake Sway.
pub trait Launcher {
    /// Start a window with `app_id` and `title` that runs `command` through
    /// the wrapper. Returns once the terminal process has started.
    ///
    /// # Errors
    /// If the terminal cannot be started.
    fn spawn(&self, app_id: &str, title: &str, command: &WindowCommand) -> Result<()>;
}

/// How an emulator takes the app id and the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Emulator {
    /// `--app-id`; the command follows the options.
    Foot,
    /// `--class`; the command follows `-e`.
    Alacritty,
    /// `--class`; the command follows the options.
    Kitty,
    /// Assumed: `--app-id`; the command follows `-e`.
    Other,
}

impl Emulator {
    fn of(program: &str) -> Self {
        match Path::new(program).file_name().and_then(|n| n.to_str()) {
            Some("foot") => Self::Foot,
            Some("alacritty") => Self::Alacritty,
            Some("kitty") => Self::Kitty,
            _ => Self::Other,
        }
    }

    fn app_id_flag(self) -> &'static str {
        match self {
            Self::Foot | Self::Other => "--app-id",
            Self::Alacritty | Self::Kitty => "--class",
        }
    }

    fn needs_dash_e(self) -> bool {
        matches!(self, Self::Alacritty | Self::Other)
    }
}

/// The argv that starts `program` with an optional app id and title,
/// running `command` if it is not empty.
fn window_argv(
    program: &str,
    app_id: Option<&str>,
    title: Option<&str>,
    command: &[String],
) -> Vec<String> {
    let emulator = Emulator::of(program);
    let mut argv = vec![program.to_owned()];
    if let Some(app_id) = app_id {
        argv.extend([emulator.app_id_flag().to_owned(), app_id.to_owned()]);
    }
    if let Some(title) = title {
        argv.extend(["-T".to_owned(), title.to_owned()]);
    }
    if !command.is_empty() {
        if emulator.needs_dash_e() {
            argv.push("-e".to_owned());
        }
        argv.extend_from_slice(command);
    }
    argv
}

/// Starts windows of the user's terminal emulator.
#[derive(Debug, Clone)]
pub struct TerminalLauncher {
    program: String,
    wrapper: PathBuf,
}

impl TerminalLauncher {
    /// Write the wrapper into `private_dir` (created 0700 by the caller) and
    /// return a launcher for `program`.
    ///
    /// # Errors
    /// If the wrapper cannot be written.
    pub fn install(program: String, private_dir: &Path) -> Result<Self> {
        let wrapper = private_dir.join(WRAPPER_FILE);
        if fs::read_to_string(&wrapper).ok().as_deref() != Some(WRAPPER) {
            write_executable(&wrapper, WRAPPER)?;
        }
        Ok(Self { program, wrapper })
    }

    /// Start a plain terminal (no i3mux) that runs `cmd` with `sh -c`, or
    /// the user's shell.
    ///
    /// # Errors
    /// If the terminal cannot be started.
    pub fn spawn_plain(&self, cmd: Option<&str>) -> Result<()> {
        let command: Vec<String> = cmd
            .map(|c| vec!["sh".to_owned(), "-c".to_owned(), c.to_owned()])
            .unwrap_or_default();
        spawn_detached(&window_argv(&self.program, None, None, &command))
    }
}

impl Launcher for TerminalLauncher {
    fn spawn(&self, app_id: &str, title: &str, command: &WindowCommand) -> Result<()> {
        spawn_detached(&window_argv(
            &self.program,
            Some(app_id),
            Some(title),
            &wrapper_argv(&self.wrapper, title, command),
        ))
    }
}

/// `wrapper.sh [--remote HOST SHELL UNIT N REATTACH...] TITLE ARGV...`, the
/// wrapper's only command line (see the module docs).
fn wrapper_argv(wrapper: &Path, title: &str, command: &WindowCommand) -> Vec<String> {
    let mut argv = vec![wrapper.display().to_string()];
    if let WindowCommand::Remote(remote) = command {
        argv.extend([
            "--remote".to_owned(),
            remote.host.to_string(),
            remote.shell.to_string(),
            remote.unit.to_string(),
            remote.reattach.len().to_string(),
        ]);
        argv.extend_from_slice(&remote.reattach);
    }
    argv.push(title.to_owned());
    argv.extend_from_slice(command.argv());
    argv
}

fn spawn_detached(argv: &[String]) -> Result<()> {
    let (program, arguments) = argv
        .split_first()
        .context("i3mux bug: empty terminal command")?;
    Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| {
            format!("cannot start terminal '{program}'; set $TERMINAL to foot, alacritty or kitty")
        })?;
    Ok(())
}

/// Write `content` to `path` with mode 0700, via a temp file and rename.
fn write_executable(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
        file.write_all(content.as_bytes())?;
        file.set_permissions(fs::Permissions::from_mode(0o700))?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn emulator_conventions() {
        let cmd = strings(&["/run/i3mux/wrapper.sh", "t", "shpool"]);
        assert_eq!(
            window_argv("foot", Some("id"), Some("t"), &cmd),
            strings(&[
                "foot",
                "--app-id",
                "id",
                "-T",
                "t",
                "/run/i3mux/wrapper.sh",
                "t",
                "shpool"
            ])
        );
        assert_eq!(
            window_argv("/usr/bin/alacritty", Some("id"), Some("t"), &cmd),
            strings(&[
                "/usr/bin/alacritty",
                "--class",
                "id",
                "-T",
                "t",
                "-e",
                "/run/i3mux/wrapper.sh",
                "t",
                "shpool"
            ])
        );
        assert_eq!(
            window_argv("kitty", Some("id"), Some("t"), &cmd)[..5],
            strings(&["kitty", "--class", "id", "-T", "t"])
        );
        assert_eq!(
            window_argv("wezterm", Some("id"), None, &cmd)[..4],
            strings(&["wezterm", "--app-id", "id", "-e"])
        );
    }

    #[test]
    fn plain_terminal_without_command_has_no_options() {
        assert_eq!(window_argv("foot", None, None, &[]), ["foot"]);
    }

    #[test]
    fn install_writes_private_executable_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = TerminalLauncher::install("foot".into(), dir.path()).unwrap();
        let mode = fs::metadata(&launcher.wrapper)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        assert_eq!(fs::read_to_string(&launcher.wrapper).unwrap(), WRAPPER);
        // A second install leaves the file alone.
        TerminalLauncher::install("foot".into(), dir.path()).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// Fake `systemctl`: `show` prints `$FAKE/unit.N` on its Nth call, else
    /// `$FAKE/unit`. Every call is logged to `$FAKE/calls`.
    const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
printf 'systemctl %s\n' "$*" >> "$FAKE/calls"
if [ "$2" = show ]; then
  n=$(( $(cat "$FAKE/shows" 2>/dev/null || echo 0) + 1 ))
  echo "$n" > "$FAKE/shows"
  if [ -e "$FAKE/unit.$n" ]; then cat "$FAKE/unit.$n"; else cat "$FAKE/unit"; fi
fi
"#;

    /// Fake `journalctl`: prints `$FAKE/journal`, the last line ssh logged.
    const FAKE_JOURNALCTL: &str = r#"#!/bin/sh
printf 'journalctl %s\n' "$*" >> "$FAKE/calls"
cat "$FAKE/journal" 2>/dev/null
exit 0
"#;

    /// Write an executable from a child process: a write descriptor held by
    /// this multi-threaded test process could leak into another test's fork
    /// and make exec fail with ETXTBSY.
    fn write_script(path: &Path, content: &str) {
        let status = Command::new("sh")
            .arg("-c")
            .arg("printf '%s' \"$2\" > \"$1\" && chmod 755 \"$1\"")
            .arg("sh")
            .arg(path)
            .arg(content)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// The installed wrapper, fake `systemctl` and `journalctl` first on
    /// `PATH`, and the files they read (`$FAKE/unit*`, `$FAKE/journal`) and
    /// write (`$FAKE/calls`). Test commands log their runs to `$FAKE/runs`.
    struct WrapperWorld {
        dir: tempfile::TempDir,
        wrapper: PathBuf,
    }

    impl WrapperWorld {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let wrapper = TerminalLauncher::install("foot".into(), dir.path())
                .unwrap()
                .wrapper;
            let bin = dir.path().join("bin");
            fs::create_dir_all(&bin).unwrap();
            fs::create_dir_all(dir.path().join("fake")).unwrap();
            write_script(&bin.join("systemctl"), FAKE_SYSTEMCTL);
            write_script(&bin.join("journalctl"), FAKE_JOURNALCTL);
            Self { dir, wrapper }
        }

        fn fake(&self, file: &str) -> PathBuf {
            self.dir.path().join("fake").join(file)
        }

        /// The unit's properties for `show` call number `call`, or for every
        /// call without its own file.
        fn set_unit(&self, call: Option<u32>, active: &str, sub: &str, inv: &str, restarts: u32) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let file = call.map_or_else(|| "unit".to_owned(), |n| format!("unit.{n}"));
            fs::write(
                self.fake(&file),
                format!(
                    "ActiveState={active}\nSubState={sub}\nInvocationID={inv}\n\
                     NRestarts={restarts}\nRestartUSecNext=8.5s\nStateChangeTimestamp=@{now}\n"
                ),
            )
            .unwrap();
        }

        fn read(&self, file: &str) -> String {
            fs::read_to_string(self.fake(file)).unwrap_or_default()
        }

        /// Run the window. `input` is typed at once; stdin then stays open
        /// for `keep_open`, so the wrapper can wait for keys, and closes.
        fn run(
            &self,
            command: &WindowCommand,
            input: &str,
            keep_open: Duration,
        ) -> std::process::Output {
            let path = format!(
                "{}:{}",
                self.dir.path().join("bin").display(),
                std::env::var("PATH").unwrap_or_default()
            );
            // Run it with bash rather than exec it: ETXTBSY, as above.
            let mut child = Command::new("bash")
                .args(wrapper_argv(&self.wrapper, "t", command))
                .env("FAKE", self.dir.path().join("fake"))
                .env("PATH", path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(input.as_bytes()).unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(keep_open);
                drop(stdin);
            });
            child.wait_with_output().unwrap()
        }
    }

    /// An attach argv that logs `label` to `$FAKE/runs`, then runs `then`.
    fn attach(label: &str, then: &str) -> Vec<String> {
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!("printf '%s\\n' \"$1\" >> \"$FAKE/runs\"; {then}"),
            "sh".to_owned(),
            label.to_owned(),
        ]
    }

    fn remote(argv: Vec<String>, reattach: Vec<String>) -> WindowCommand {
        let ws4 = crate::types::SessionName::new("ws4").unwrap();
        WindowCommand::Remote(RemoteAttach {
            argv,
            reattach,
            unit: UnitName::from_escaped_host("deepthought").unwrap(),
            host: RemoteHost::new("deepthought").unwrap(),
            shell: TerminalName::new(&ws4, 2),
        })
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    use std::time::Duration;
    const AT_ONCE: Duration = Duration::ZERO;

    #[test]
    fn remote_window_puts_its_recovery_words_before_the_title() {
        let command = remote(strings(&["ssh", "a b"]), strings(&["ssh", "-f", "c'd"]));
        assert_eq!(
            wrapper_argv(Path::new("/w.sh"), "i3mux:deepthought:ws4-002", &command),
            strings(&[
                "/w.sh",
                "--remote",
                "deepthought",
                "ws4-002",
                "i3mux-ssh-deepthought.service",
                "3",
                "ssh",
                "-f",
                "c'd",
                "i3mux:deepthought:ws4-002",
                "ssh",
                "a b",
            ])
        );
        let plain = WindowCommand::Plain(strings(&["x"]));
        assert_eq!(
            wrapper_argv(Path::new("/w.sh"), "t", &plain),
            ["/w.sh", "t", "x"]
        );
    }

    #[test]
    fn wrapper_sets_title_and_runs_argv_unparsed() {
        let world = WrapperWorld::new();
        let argv = strings(&["printf", "%s", "a'b \"$HOME\""]);
        let out = world.run(&WindowCommand::Plain(argv), "", AT_ONCE);
        assert!(out.status.success());
        let text = text(&out.stdout);
        assert!(text.starts_with("\x1b]0;t\x07a'b \"$HOME\"\n"), "{text:?}");
    }

    #[test]
    fn wrapper_pauses_with_hint_on_failure() {
        let world = WrapperWorld::new();
        let argv = strings(&["sh", "-c", "exit 3"]);
        let out = world.run(&WindowCommand::Plain(argv), "\n", AT_ONCE);
        assert_eq!(out.status.code(), Some(3));
        let text = text(&out.stdout);
        assert!(text.contains("with exit status 3"), "{text}");
        assert!(
            text.contains("'i3mux terminal --force' or 'i3mux attach --force'"),
            "{text}"
        );
        assert!(text.contains("Press Enter"), "{text}");
    }

    #[test]
    fn wrapper_pauses_when_the_command_ends_at_once() {
        // Plan rule 10: shpool refuses a second client with exit status 0,
        // so a quick end must keep the window and its hint on screen.
        let world = WrapperWorld::new();
        let out = world.run(&WindowCommand::Plain(strings(&["true"])), "\n", AT_ONCE);
        assert_eq!(out.status.code(), Some(0));
        let text = text(&out.stdout);
        assert!(
            text.contains("already attached from another terminal or host"),
            "{text}"
        );
        assert!(text.contains("Press Enter"), "{text}");
    }

    #[test]
    fn wrapper_closes_quietly_after_a_long_successful_run() {
        let world = WrapperWorld::new();
        let argv = strings(&["sleep", "2"]);
        let out = world.run(&WindowCommand::Plain(argv), "", AT_ONCE);
        assert!(out.status.success());
        assert_eq!(text(&out.stdout), "\x1b]0;t\x07");
    }

    #[test]
    fn local_window_never_asks_systemd() {
        // Plan-ssh-recovery rule 14: exit 255 of a local window keeps the hint.
        let world = WrapperWorld::new();
        let argv = strings(&["sh", "-c", "exit 255"]);
        let out = world.run(&WindowCommand::Plain(argv), "\n", AT_ONCE);
        assert_eq!(out.status.code(), Some(255));
        assert!(text(&out.stdout).contains("with exit status 255."));
        assert_eq!(world.read("calls"), "");
    }

    #[test]
    fn remote_window_reattaches_with_force_once_the_master_restarted() {
        // Rule 8: exit 255 and a new InvocationID mean that the link dropped.
        // The reattach argv also arrives unparsed.
        let world = WrapperWorld::new();
        world.set_unit(Some(1), "active", "running", "A", 0);
        world.set_unit(None, "active", "running", "B", 1);
        let command = remote(
            attach("plain", "exit 255"),
            attach("force a'b \"$HOME\"", "sleep 2"),
        );
        let out = world.run(&command, "", AT_ONCE);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
        assert_eq!(world.read("runs"), "plain\nforce a'b \"$HOME\"\n");
        assert!(!text(&out.stdout).contains("lost"), "no landing page");
    }

    #[test]
    fn remote_window_with_the_same_invocation_shows_the_error_and_never_loops() {
        // Rule 8: exit 255 while the unit stays active with the same
        // InvocationID. The master never dropped, so ssh failed for another
        // reason, and an automatic reattach would fail the same way forever.
        let world = WrapperWorld::new();
        world.set_unit(None, "active", "running", "A", 0);
        let command = remote(
            attach(
                "plain",
                "echo 'Host key verification failed.' >&2; exit 255",
            ),
            attach("force", "exit 0"),
        );
        let out = world.run(&command, "", Duration::from_millis(2500));
        assert_eq!(out.status.code(), Some(255));
        assert_eq!(world.read("runs"), "plain\n", "no reattach without a key");
        let text = text(&out.stdout);
        for line in [
            "[i3mux] ssh to deepthought failed with exit status 255.",
            "[i3mux] The connection to deepthought is up, so the network did not drop.",
            "[i3mux] Host key verification failed.",
            "[i3mux] Enter: retry   q: close window",
        ] {
            assert!(text.contains(line), "{line:?} missing from {text}");
        }
        assert!(!world.read("calls").contains("restart"));
    }

    #[test]
    fn remote_window_shows_the_landing_page_until_the_master_is_back() {
        // Rules 7 and 8: exit 255 while systemd restarts the master.
        let world = WrapperWorld::new();
        world.set_unit(Some(1), "active", "running", "A", 4);
        world.set_unit(Some(2), "activating", "auto-restart", "A", 4);
        world.set_unit(None, "active", "running", "B", 5);
        fs::write(
            world.fake("journal"),
            "ssh: connect to host deepthought port 22: Network is unreachable\n",
        )
        .unwrap();
        let command = remote(attach("plain", "exit 255"), attach("force", "sleep 2"));
        let out = world.run(&command, "", Duration::from_secs(10));
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(world.read("runs"), "plain\nforce\n");
        let text = text(&out.stdout);
        for line in [
            "[i3mux] Connection to deepthought lost. Shell ws4-002 is safe on the host.\n",
            "[i3mux] ssh: connect to host deepthought port 22: Network is unreachable\n",
            "[i3mux] Reconnecting: attempt 1, next try in ",
            "[i3mux] Enter: retry now   q: close window\n",
            "[i3mux] Log: journalctl --user -u i3mux-ssh-deepthought.service\n",
        ] {
            assert!(text.contains(line), "{line:?} missing from {text}");
        }
        assert!(world
            .read("calls")
            .contains("journalctl --user -u i3mux-ssh-deepthought.service -t ssh -n 1"));
    }

    #[test]
    fn landing_page_enter_restarts_the_unit_and_q_closes_the_window() {
        // Rule 11.
        let world = WrapperWorld::new();
        world.set_unit(Some(1), "active", "running", "A", 0);
        world.set_unit(None, "inactive", "dead", "", 0);
        let command = remote(attach("plain", "exit 255"), attach("force", "exit 0"));
        let out = world.run(&command, "\nq", Duration::from_secs(5));
        assert_eq!(out.status.code(), Some(255));
        assert_eq!(world.read("runs"), "plain\n");
        let calls = world.read("calls");
        assert_eq!(calls.matches(" restart ").count(), 1, "{calls}");
        assert!(
            calls.contains("systemctl --user restart --no-block i3mux-ssh-deepthought.service\n")
        );
        assert!(text(&out.stdout)
            .contains("[i3mux] Not reconnecting: the connection unit is inactive."));
    }

    #[test]
    fn remote_window_starts_one_new_shell_after_the_daemon_was_replaced() {
        // Rule 9: exit 75 reattaches once. A second 75 keeps today's hint.
        let world = WrapperWorld::new();
        world.set_unit(None, "active", "running", "A", 0);
        let command = remote(attach("plain", "exit 75"), attach("force", "exit 0"));
        let out = world.run(&command, "\n", AT_ONCE);
        assert_eq!(out.status.code(), Some(75));
        assert_eq!(world.read("runs"), "plain\nplain\n");
        let text = text(&out.stdout);
        assert!(
            text.contains("[i3mux] Starting a new shell in this window.\n"),
            "{text}"
        );
        assert!(text.contains("with exit status 75."), "{text}");
    }

    #[test]
    fn remote_window_keeps_todays_hint_for_any_other_exit() {
        // Rule 10: a client that lost its shell to --force exits 1, not 255.
        let world = WrapperWorld::new();
        world.set_unit(None, "active", "running", "A", 0);
        let command = remote(
            attach(
                "plain",
                "echo 'Shared connection to deepthought closed.' >&2; exit 1",
            ),
            attach("force", "exit 0"),
        );
        let out = world.run(&command, "\n", AT_ONCE);
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(world.read("runs"), "plain\n");
        let stdout = text(&out.stdout);
        assert!(stdout.contains("with exit status 1."), "{stdout}");
        assert!(stdout.contains("'i3mux terminal --force'"), "{stdout}");
        assert!(text(&out.stderr).contains("Shared connection to deepthought closed."));
        assert!(!world.read("calls").contains("restart"));
    }
}
