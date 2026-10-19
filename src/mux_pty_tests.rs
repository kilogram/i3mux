//! The shpool commands i3mux generates, run over a pty against a real shpool
//! on a scratch `HOME`.
//!
//! These pin the multiplexer behaviour the plan is built on (findings 1, 2
//! and 4, rules 10 and 16): a reattach replays the whole history, never
//! switches to the alternate screen, and shpool refuses a second client
//! unless `--force`, with exit status 0 and at once, which the terminal
//! wrapper turns into a window that stays open with the `--force` hint.
//! They also pin the recovery from a daemon that was killed.
//!
//! Ignored by default because they need shpool: `just check-mux` runs them.
//! The binary is `$SHPOOL_BIN`, else `shpool` on `PATH`. A missing binary
//! fails the tests; it never skips them.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::{openpty, Winsize};

use super::{attach_argv, detach, list, parse_list, MuxError, Shell};
use crate::host::{HostRunner, RunnerConfig};
use crate::types::{Host, SessionName, TerminalName};

/// Sequences that switch a terminal to the alternate screen. foot and
/// alacritty do not scroll it (plan finding 2).
const ALT_SCREEN: [&str; 3] = ["\x1b[?1049h", "\x1b[?1047h", "\x1b[?47h"];

/// Up to the first attach, shpool starts its daemon; allow for a slow CI.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Prints `LINE-001` to `LINE-200`, then a marker, then idles. shpool splits
/// this with shell-words rules, as it does for `i3mux terminal -e`.
const PRINT_200: &str = r#"sh -c 'i=1; while [ "$i" -le 200 ]; do printf "LINE-%03d\n" "$i"; i=$((i+1)); done; echo DONE-PRINTING; exec sleep 600'"#;

/// The shpool binary under test.
fn shpool_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("SHPOOL_BIN").filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "SHPOOL_BIN={} is not a file",
            path.display()
        );
        return path;
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join("shpool"))
                .find(|p| p.is_file())
        })
        .unwrap_or_else(|| {
            panic!(
                "shpool not found: set SHPOOL_BIN or put shpool on PATH. \
                 These tests fail rather than skip when shpool is missing."
            )
        })
}

/// A scratch `HOME` with its own shpool daemon. Dropping it kills the daemon
/// and every shell it started.
struct Scratch {
    home: tempfile::TempDir,
    shpool: PathBuf,
    runner: HostRunner,
}

impl Scratch {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // The generated argv runs `shpool` from PATH; PATH gets only this dir
        // and the system dirs, so the binary under test is the one found.
        let shpool = bin.join("shpool");
        std::os::unix::fs::symlink(shpool_bin(), &shpool).unwrap();
        std::fs::create_dir_all(home.path().join("xdg")).unwrap();
        let runner = HostRunner::new(
            Host::Local,
            RunnerConfig {
                local_home: home.path().to_path_buf(),
                ssh_control_dir: home.path().join("ssh"),
                masters: std::sync::Arc::new(crate::master::DirectSsh),
            },
        );
        Self {
            home,
            shpool,
            runner,
        }
    }

    fn path(&self) -> &Path {
        self.home.path()
    }

    fn socket(&self) -> PathBuf {
        self.path().join(".local/run/i3mux/shpool.socket")
    }

    /// A hermetic environment: nothing from the developer's shell setup.
    fn env(&self, command: &mut Command) {
        command
            .env_clear()
            .env("HOME", self.path())
            .env("XDG_RUNTIME_DIR", self.path().join("xdg"))
            .env(
                "PATH",
                format!(
                    "{}:/usr/local/bin:/usr/bin:/bin",
                    self.path().join("bin").display()
                ),
            )
            .env("TERM", "xterm-256color")
            .env("SHELL", "/bin/sh")
            .env(
                "USER",
                std::env::var("USER").unwrap_or_else(|_| "i3mux-test".into()),
            );
    }

    /// Run shpool directly on the i3mux socket (for detach and list).
    fn shpool(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.shpool);
        self.env(&mut command);
        command
            .arg("--socket")
            .arg(self.socket())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `shpool list --json`, parsed by i3mux's own parser.
    fn list(&self) -> Vec<Shell> {
        let out = self.shpool(&["list", "--json"]);
        assert!(out.status.success(), "shpool list: {out:?}");
        parse_list(&String::from_utf8_lossy(&out.stdout)).unwrap()
    }

    fn shell(&self, name: &TerminalName) -> Option<Shell> {
        self.list().into_iter().find(|s| s.name() == name)
    }

    /// Wait until `name` is attached (`true`) or disconnected (`false`).
    fn wait_attached(&self, name: &TerminalName, attached: bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let shell = self.shell(name);
            if shell.as_ref().is_some_and(|s| s.is_attached() == attached) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{name} never became attached={attached}; list: {:?}",
                self.list()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Run the exact argv i3mux gives a terminal window, in a new pty.
    fn attach(&self, name: &TerminalName, force: bool, cmd: Option<&str>) -> PtyClient {
        let argv = attach_argv(&self.runner, name, force, cmd);
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        self.env(&mut command);
        PtyClient::spawn(command)
    }

    /// Run what an i3mux window runs: the real wrapper script with the
    /// attach argv, in a new pty. (Run with `bash`, not exec'd: a freshly
    /// written file can be ETXTBSY in a multi-threaded test process.)
    fn attach_in_wrapper(&self, name: &TerminalName) -> PtyClient {
        let wrapper = self.path().join("wrapper.sh");
        std::fs::write(&wrapper, crate::launch::WRAPPER).unwrap();
        let mut command = Command::new("bash");
        command
            .arg(&wrapper)
            .arg(format!("i3mux:local:{name}"))
            .args(attach_argv(&self.runner, name, false, None));
        self.env(&mut command);
        PtyClient::spawn(command)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // The daemon and its shells have the scratch path on their command
        // line or in their environment; the daemon's does.
        let _ = Command::new("pkill")
            .arg("-f")
            .arg(self.path().as_os_str())
            .status();
    }
}

/// A process on the slave side of a pty whose output is collected.
struct PtyClient {
    child: Child,
    output: Arc<Mutex<Vec<u8>>>,
    /// The master side, for typing into the process.
    keyboard: File,
}

impl PtyClient {
    fn spawn(mut command: Command) -> Self {
        // A 0x0 pty makes shpool's client exit at once, so give it a size.
        let size = Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(&size, None).unwrap();
        let slave = File::from(pty.slave);
        command
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        let child = command.spawn().unwrap();
        // Drop the parent's slave copies so the reader sees EOF when the
        // child exits.
        drop(command);

        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let mut master = File::from(pty.master);
        let keyboard = master.try_clone().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            // EIO once the slave side is closed ends the loop.
            while let Ok(n) = master.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self {
            child,
            output,
            keyboard,
        }
    }

    fn type_text(&mut self, text: &str) {
        self.keyboard.write_all(text.as_bytes()).unwrap();
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Wait until the output contains `needle`.
    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.text().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "no {needle:?} within {TIMEOUT:?}; output:\n{:?}",
                self.text()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // Let the reader drain what the child wrote last.
                thread::sleep(Duration::from_millis(200));
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "client did not exit within {TIMEOUT:?}; output:\n{:?}",
                self.text()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for PtyClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn terminal() -> TerminalName {
    TerminalName::new(&SessionName::new("ws4").unwrap(), 1)
}

fn assert_no_alt_screen(what: &str, text: &str) {
    for seq in ALT_SCREEN {
        assert!(
            !text.contains(seq),
            "{what} switched to the alternate screen ({seq:?}); output:\n{text:?}"
        );
    }
}

#[test]
#[ignore = "needs shpool; run with `just check-mux` (plan, Tests: Multiplexer)"]
fn reattach_replays_full_history_without_alternate_screen() {
    let scratch = Scratch::new();
    let name = terminal();

    let mut first = scratch.attach(&name, false, Some(PRINT_200));
    first.wait_for("DONE-PRINTING");
    assert_no_alt_screen("the first attach", &first.text());

    let out = scratch.shpool(&["detach", &name.to_string()]);
    assert!(out.status.success(), "shpool detach: {out:?}");
    first.wait_exit();
    scratch.wait_attached(&name, false);

    // An orphan reattach: no command, exactly as `i3mux terminal` runs it.
    let second = scratch.attach(&name, false, None);
    second.wait_for("DONE-PRINTING");
    let replay = second.text();
    // LINE-001 scrolled off a 24-row screen long ago: only a history replay
    // (session_restore_mode = lines) brings it back, not a screen redraw.
    for line in ["LINE-001", "LINE-100", "LINE-200"] {
        assert!(
            replay.contains(line),
            "{line} was not replayed; output:\n{replay:?}"
        );
    }
    assert_no_alt_screen("the reattach", &replay);
}

#[test]
#[ignore = "needs shpool; run with `just check-mux` (plan rule 10)"]
fn second_client_is_refused_unless_forced() {
    let scratch = Scratch::new();
    let name = terminal();

    let mut first = scratch.attach(&name, false, Some(PRINT_200));
    first.wait_for("DONE-PRINTING");
    scratch.wait_attached(&name, true);

    // A plain second attach is refused and leaves the first client alone.
    let mut refused = scratch.attach(&name, false, None);
    refused.wait_exit();
    assert!(
        refused.text().contains("already has a terminal attached"),
        "not a refusal; output:\n{:?}",
        refused.text()
    );
    assert!(
        first.is_running(),
        "the refused attach disturbed the first client"
    );
    assert!(scratch.shell(&name).unwrap().is_attached());

    // --force takes the shell over and disconnects the first client.
    let forced = scratch.attach(&name, true, None);
    forced.wait_for("DONE-PRINTING");
    first.wait_exit();
    scratch.wait_attached(&name, true);
    assert!(
        scratch.list().len() == 1,
        "force created a second shell: {:?}",
        scratch.list()
    );
}

/// shpool 0.11.5 refuses a second client with exit status 0 and at once.
/// The wrapper cannot tell that from a shell that ended, so it keeps any
/// window whose command ended within 2 s open, with the `--force` hint
/// (plan rule 10). This runs the real wrapper with the refused attach argv.
#[test]
#[ignore = "needs shpool; run with `just check-mux` (plan rule 10)"]
fn refused_attach_exits_zero_at_once_and_the_wrapper_keeps_the_window_open() {
    let scratch = Scratch::new();
    let name = terminal();
    let mut first = scratch.attach(&name, false, Some(PRINT_200));
    first.wait_for("DONE-PRINTING");
    scratch.wait_attached(&name, true);

    let mut window = scratch.attach_in_wrapper(&name);
    window.wait_for("Press Enter");
    let text = window.text();
    assert!(
        text.contains("already has a terminal attached"),
        "not a refusal; output:\n{text:?}"
    );
    // The wrapper reports the command's status: shpool's refusal is exit 0.
    // With status 0 the hint appears only for an end within 2 s.
    assert!(
        text.contains("with exit status 0."),
        "shpool's refusal status changed; output:\n{text:?}"
    );
    assert!(
        text.contains("'i3mux terminal --force' or 'i3mux attach --force'"),
        "no --force hint; output:\n{text:?}"
    );
    assert!(window.is_running(), "the window must wait for Enter");

    window.type_text("\n");
    assert!(window.wait_exit().success());
    assert!(first.is_running(), "the refusal disturbed the first client");
}

/// `i3mux detach` relies on this: one `shpool detach` call with several
/// names disconnects every client, the shells keep running, and a second
/// call (nothing attached any more) is not an error.
#[test]
#[ignore = "needs shpool; run with `just check-mux` (detach before attach)"]
fn detach_disconnects_several_shells_in_one_call() {
    let scratch = Scratch::new();
    let ws4 = SessionName::new("ws4").unwrap();
    let names = [TerminalName::new(&ws4, 1), TerminalName::new(&ws4, 2)];
    let mut clients: Vec<PtyClient> = names
        .iter()
        .map(|name| scratch.attach(name, false, Some(PRINT_200)))
        .collect();
    for (client, name) in clients.iter().zip(&names) {
        client.wait_for("DONE-PRINTING");
        scratch.wait_attached(name, true);
    }

    detach(&scratch.runner, &names).unwrap();

    for (client, name) in clients.iter_mut().zip(&names) {
        client.wait_exit();
        scratch.wait_attached(name, false);
    }
    detach(&scratch.runner, &names).unwrap();
    assert_eq!(scratch.list().len(), 2, "detach must not end the shells");
}

/// A daemon killed with SIGKILL leaves its socket file behind, and shpool
/// fails on it with no output. The next i3mux command starts a new daemon
/// and fails with `DaemonRestarted`, the one after works, and the log keeps
/// the output of both daemons.
#[test]
#[ignore = "needs shpool; run with `just check-mux` (dead daemon recovery)"]
fn killed_daemon_costs_one_command_and_its_log_survives() {
    let scratch = Scratch::new();
    let name = terminal();
    let mut client = scratch.attach(&name, false, Some(PRINT_200));
    client.wait_for("DONE-PRINTING");

    let daemon = format!("{} --config-file .* daemon", scratch.socket().display());
    let status = Command::new("pkill")
        .args(["-KILL", "-f", &daemon])
        .status()
        .unwrap();
    assert!(status.success(), "no process matched {daemon:?}");
    client.wait_exit();
    assert!(
        scratch.socket().exists(),
        "SIGKILL must leave the socket file"
    );

    let err = list(&scratch.runner).unwrap_err();
    assert!(matches!(err, MuxError::DaemonRestarted { .. }), "{err}");
    assert_eq!(list(&scratch.runner).unwrap(), []);
    let log =
        std::fs::read_to_string(scratch.path().join(".local/state/i3mux/shpool.log")).unwrap();
    assert_eq!(log.matches("starting a shpool daemon").count(), 2, "{log}");
    assert!(log.contains("does not answer"), "{log}");
}
