//! Launch terminal emulator windows.
//!
//! An i3mux window runs the wrapper in `$TERMINAL`:
//!
//! ```text
//! wrapper.sh [--no-hold] [--attach-only] I3MUX HOST SHELL TITLE ARGV...
//! wrapper.sh [--no-hold] [--attach-only] I3MUX HOST SHELL --remote UNIT N REATTACH... TITLE ARGV...
//! ```
//!
//! The wrapper sets the title once, runs ARGV with no shell, and keeps the
//! window open if ARGV fails (plan decisions 10 and 13), unless
//! [`WrapperOptions::no_hold`]. A remote window
//! also recovers from a lost ssh connection: it shows a landing page while
//! UNIT, the host's ssh master, reconnects, then runs REATTACH, the N-word
//! argv that attaches SHELL with `--force` (plan-ssh-recovery rules 7-11).
//!
//! Every window follows its session to another host (PLAN-migrate decision
//! 9). Before each rerun of its attach, and after its last end, the wrapper
//! asks `I3MUX reattach --shell SHELL --from HOST` where the session is.
//! HOST is a host label: `local`, or the remote host. When the session is
//! bound to another host, reattach prints a wrapper argv for that host and
//! the wrapper execs it. If that host does not answer, the new wrapper's
//! own ssh recovery waits for it. Any other status ([`EXIT_STAY`]) keeps
//! the window on HOST.
//!
//! [`TerminalLauncher::wrapper_argv`] is the only code that builds this
//! command line. Every word reaches the wrapper as one argument, so nothing
//! is quoted.
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
use crate::window::Mark;

/// The wrapper script, written to the private runtime directory.
pub const WRAPPER: &str = include_str!("wrapper.sh");
const WRAPPER_FILE: &str = "wrapper.sh";

/// Exit status of `i3mux reattach` when the window stays on its host: its
/// session is bound there, nowhere, or on two other hosts. The wrapper
/// then takes its usual path. It treats every status other than 0 the
/// same way.
pub const EXIT_STAY: u8 = 7;

/// What an i3mux window runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowCommand {
    /// Run the argv: a local shell, or a remote one on a machine that runs
    /// no ssh masters.
    Plain(Vec<String>),
    /// Attach a remote shell through its host's ssh master, and recover when
    /// the connection drops.
    Remote(RemoteAttach),
    /// The binding's window command (PLAN-terminal-here decision 2), run
    /// as it is, without the wrapper.
    Custom(Vec<String>),
}

impl WindowCommand {
    /// The argv the window runs first.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        match self {
            Self::Plain(argv) | Self::Custom(argv) => argv,
            Self::Remote(remote) => &remote.argv,
        }
    }
}

/// A remote window, and what it needs to recover from a lost connection.
/// Its host and its shell are those of the window's mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAttach {
    /// The attach argv.
    pub argv: Vec<String>,
    /// The attach argv with `--force`, run once the connection is back: the
    /// host may still count the lost client as attached.
    pub reattach: Vec<String>,
    /// The ssh master unit of the window's host, which the window watches.
    pub unit: UnitName,
}

/// How the wrapper ends a window. The default keeps today's behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WrapperOptions {
    /// End the window with the shell's exit status when the shell ends or
    /// is detached, never keeping it open with a hint (PLAN-terminal-here
    /// decision 4).
    pub no_hold: bool,
    /// The argv attaches an existing shell only: after exit 75 (the shpool
    /// daemon was replaced) the wrapper does not run it again, since that
    /// would start a new shell under the same name.
    pub attach_only: bool,
}

/// Opens terminal windows. [`TerminalLauncher`] is the real one; tests use
/// fakes that add windows to a fake Sway.
pub trait Launcher {
    /// Start the window of `mark`, with the mark's app id and title, that
    /// runs `command` through the wrapper, with `options`. Returns once the
    /// terminal process has started.
    ///
    /// # Errors
    /// If the terminal cannot be started.
    fn spawn(&self, mark: &Mark, command: &WindowCommand, options: WrapperOptions) -> Result<()>;

    /// The argv that the window of `mark` runs to show `command`: the
    /// wrapper with `options`, or a window command as it is. `spawn` runs
    /// it in a new terminal, `terminal --here` execs it, and a wrapper
    /// execs it when the window follows its session to another host.
    fn program_argv(
        &self,
        mark: &Mark,
        command: &WindowCommand,
        options: WrapperOptions,
    ) -> Vec<String>;
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
    /// The i3mux binary that each wrapper asks where its session is.
    i3mux: PathBuf,
}

impl TerminalLauncher {
    /// Write the wrapper into `private_dir` (created 0700 by the caller) and
    /// return a launcher for `program`, whose wrappers run `i3mux`.
    ///
    /// # Errors
    /// If the wrapper cannot be written.
    pub fn install(program: String, i3mux: PathBuf, private_dir: &Path) -> Result<Self> {
        let wrapper = private_dir.join(WRAPPER_FILE);
        if fs::read_to_string(&wrapper).ok().as_deref() != Some(WRAPPER) {
            write_executable(&wrapper, WRAPPER)?;
        }
        Ok(Self {
            program,
            wrapper,
            i3mux,
        })
    }

    /// The terminal's argv for the window of `mark`. A window of the
    /// binding's window command gets no title flag: alacritty and kitty
    /// lock a title given on their command line, and the window command
    /// may run `terminal --here`, which finds its window by a title it sets
    /// (PLAN-terminal-here). i3mux finds the window by its app id.
    fn terminal_argv(
        &self,
        mark: &Mark,
        command: &WindowCommand,
        options: WrapperOptions,
    ) -> Vec<String> {
        let argv = self.program_argv(mark, command, options);
        let title = match command {
            WindowCommand::Custom(_) => None,
            WindowCommand::Plain(_) | WindowCommand::Remote(_) => Some(mark.title()),
        };
        window_argv(&self.program, Some(&mark.app_id()), title.as_deref(), &argv)
    }

    /// `wrapper.sh [--no-hold] [--attach-only] I3MUX HOST SHELL [--remote
    /// UNIT N REATTACH...] TITLE ARGV...`: the wrapper's only command line
    /// (see the module docs).
    fn wrapper_argv(
        &self,
        mark: &Mark,
        command: &WindowCommand,
        options: WrapperOptions,
    ) -> Vec<String> {
        let mut argv = vec![self.wrapper.display().to_string()];
        if options.no_hold {
            argv.push("--no-hold".to_owned());
        }
        if options.attach_only {
            argv.push("--attach-only".to_owned());
        }
        argv.extend([
            self.i3mux.display().to_string(),
            mark.host().to_string(),
            mark.terminal().to_string(),
        ]);
        if let WindowCommand::Remote(remote) = command {
            argv.extend([
                "--remote".to_owned(),
                remote.unit.to_string(),
                remote.reattach.len().to_string(),
            ]);
            argv.extend_from_slice(&remote.reattach);
        }
        argv.push(mark.title());
        argv.extend_from_slice(command.argv());
        argv
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
    fn spawn(&self, mark: &Mark, command: &WindowCommand, options: WrapperOptions) -> Result<()> {
        spawn_detached(&self.terminal_argv(mark, command, options))
    }

    fn program_argv(
        &self,
        mark: &Mark,
        command: &WindowCommand,
        options: WrapperOptions,
    ) -> Vec<String> {
        match command {
            WindowCommand::Custom(argv) => argv.clone(),
            WindowCommand::Plain(_) | WindowCommand::Remote(_) => {
                self.wrapper_argv(mark, command, options)
            }
        }
    }
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
    use crate::types::{Host, RemoteHost, SessionName, TerminalName};

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    /// The window of shell `ws4-002` on `host`.
    fn mark_on(host: &str) -> Mark {
        let ws4 = SessionName::new("ws4").unwrap();
        Mark::new(
            Host::Ssh(RemoteHost::new(host).unwrap()),
            TerminalName::new(&ws4, 2),
        )
    }

    /// A launcher whose wrapper and i3mux are at fixed paths, for argv
    /// tests that run nothing.
    fn fixed_launcher(program: &str) -> TerminalLauncher {
        TerminalLauncher {
            program: program.to_owned(),
            wrapper: "/w.sh".into(),
            i3mux: "/bin/i3mux".into(),
        }
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

    /// alacritty ignores the titles programs set once `-T` names one, so
    /// `terminal --here` in a window command's window could not find its
    /// window (found by handoff's done-when test, real alacritty).
    #[test]
    fn a_window_commands_window_gets_no_title_flag() {
        let launcher = fixed_launcher("alacritty");
        let mark = mark_on("deepthought");
        let custom = WindowCommand::Custom(strings(&["handoff", "window", "ws3", "ws3-002"]));
        assert_eq!(
            launcher.terminal_argv(&mark, &custom, WrapperOptions::default()),
            strings(&[
                "alacritty",
                "--class",
                "_i3mux:deepthought:ws4-002",
                "-e",
                "handoff",
                "window",
                "ws3",
                "ws3-002"
            ])
        );
        let plain = WindowCommand::Plain(strings(&["shpool"]));
        let argv = launcher.terminal_argv(&mark, &plain, WrapperOptions::default());
        assert_eq!(
            argv[..5],
            strings(&[
                "alacritty",
                "--class",
                "_i3mux:deepthought:ws4-002",
                "-T",
                "i3mux:deepthought:ws4-002"
            ])
        );
    }

    #[test]
    fn plain_terminal_without_command_has_no_options() {
        assert_eq!(window_argv("foot", None, None, &[]), ["foot"]);
    }

    #[test]
    fn install_writes_private_executable_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let launcher =
            TerminalLauncher::install("foot".into(), "/bin/i3mux".into(), dir.path()).unwrap();
        let mode = fs::metadata(&launcher.wrapper)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        assert_eq!(fs::read_to_string(&launcher.wrapper).unwrap(), WRAPPER);
        // A second install leaves the file alone.
        TerminalLauncher::install("foot".into(), "/bin/i3mux".into(), dir.path()).unwrap();
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

    /// Fake `i3mux`: logs its arguments to `$FAKE/runs`, among the
    /// attaches. On its Nth call it answers with `$FAKE/follow.N`: the
    /// first line is its exit status, each further line one word it
    /// prints, NUL-terminated. With no such file the window stays (7).
    const FAKE_I3MUX: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE/runs"
n=$(( $(cat "$FAKE/follows" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$FAKE/follows"
answer="$FAKE/follow.$n"
[ -e "$answer" ] || exit 7
tail -n +2 "$answer" | tr '\n' '\0'
exit "$(head -n 1 "$answer")"
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

    /// The installed wrapper of the window of `ws4-002` on deepthought,
    /// fake `systemctl` and `journalctl` first on `PATH`, a fake `i3mux`
    /// that the wrapper runs, and the files they read (`$FAKE/unit*`,
    /// `$FAKE/journal`, `$FAKE/follow.*`) and write (`$FAKE/calls`). Test
    /// commands and the fake `i3mux` log their runs to `$FAKE/runs`.
    struct WrapperWorld {
        dir: tempfile::TempDir,
        launcher: TerminalLauncher,
        /// The window's mark.
        mark: Mark,
        /// The options the window runs with.
        options: WrapperOptions,
    }

    impl WrapperWorld {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            fs::create_dir_all(&bin).unwrap();
            fs::create_dir_all(dir.path().join("fake")).unwrap();
            write_script(&bin.join("systemctl"), FAKE_SYSTEMCTL);
            write_script(&bin.join("journalctl"), FAKE_JOURNALCTL);
            write_script(&bin.join("i3mux"), FAKE_I3MUX);
            let launcher =
                TerminalLauncher::install("foot".into(), bin.join("i3mux"), dir.path()).unwrap();
            Self {
                dir,
                launcher,
                mark: mark_on("deepthought"),
                options: WrapperOptions::default(),
            }
        }

        fn fake(&self, file: &str) -> PathBuf {
            self.dir.path().join("fake").join(file)
        }

        /// The argv that runs the wrapper of `mark`, with this world's
        /// options, through bash (ETXTBSY, as in `write_script`).
        fn wrapper_argv(&self, mark: &Mark, command: &WindowCommand) -> Vec<String> {
            let mut argv = vec!["bash".to_owned()];
            argv.extend(self.launcher.wrapper_argv(mark, command, self.options));
            argv
        }

        /// `i3mux reattach` call number `call` exits with `status`, after
        /// it printed `words`.
        fn answer_reattach(&self, call: u32, status: u8, words: &[String]) {
            let mut text = format!("{status}\n");
            for word in words {
                assert!(!word.contains('\n'), "one line per word: {word:?}");
                text.push_str(word);
                text.push('\n');
            }
            fs::write(self.fake(&format!("follow.{call}")), text).unwrap();
        }

        /// The runs logged, without the calls of the fake `i3mux`.
        fn attaches(&self) -> String {
            self.read("runs")
                .split_inclusive('\n')
                .filter(|line| !line.starts_with("reattach "))
                .collect()
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
            let argv = self.wrapper_argv(&self.mark, command);
            let mut child = Command::new(&argv[0])
                .args(&argv[1..])
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

    /// A remote window through the master of `unit_host`.
    fn remote_on(unit_host: &str, argv: Vec<String>, reattach: Vec<String>) -> WindowCommand {
        WindowCommand::Remote(RemoteAttach {
            argv,
            reattach,
            unit: UnitName::from_escaped_host(unit_host).unwrap(),
        })
    }

    fn remote(argv: Vec<String>, reattach: Vec<String>) -> WindowCommand {
        remote_on("deepthought", argv, reattach)
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    use std::time::Duration;
    const AT_ONCE: Duration = Duration::ZERO;

    /// What the fake `i3mux` logs for the window of `ws4-002` on `host`.
    fn reattach_from(host: &str) -> String {
        format!("reattach --shell ws4-002 --from {host}\n")
    }

    #[test]
    fn remote_window_puts_its_recovery_words_before_the_title() {
        let launcher = fixed_launcher("foot");
        let mark = mark_on("deepthought");
        let command = remote(strings(&["ssh", "a b"]), strings(&["ssh", "-f", "c'd"]));
        assert_eq!(
            launcher.wrapper_argv(&mark, &command, WrapperOptions::default()),
            strings(&[
                "/w.sh",
                "/bin/i3mux",
                "deepthought",
                "ws4-002",
                "--remote",
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
        let local = Mark::new(Host::Local, mark.terminal().clone());
        let plain = WindowCommand::Plain(strings(&["x"]));
        assert_eq!(
            launcher.wrapper_argv(&local, &plain, WrapperOptions::default()),
            [
                "/w.sh",
                "/bin/i3mux",
                "local",
                "ws4-002",
                "i3mux:local:ws4-002",
                "x"
            ]
        );
    }

    #[test]
    fn wrapper_sets_title_and_runs_argv_unparsed() {
        let world = WrapperWorld::new();
        let argv = strings(&["printf", "%s", "a'b \"$HOME\""]);
        let out = world.run(&WindowCommand::Plain(argv), "", AT_ONCE);
        assert!(out.status.success());
        let text = text(&out.stdout);
        assert!(
            text.starts_with("\x1b]0;i3mux:deepthought:ws4-002\x07a'b \"$HOME\"\n"),
            "{text:?}"
        );
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

    /// shell-pool/shpool#445: an attach at 0 rows or 0 columns panics
    /// shpool's shell-to-client thread, and the shell can never be attached
    /// again. On a terminal of that size the wrapper runs nothing and
    /// exits, after one line.
    #[test]
    fn wrapper_never_attaches_on_a_terminal_with_no_size() {
        use nix::pty::{openpty, Winsize};
        let world = WrapperWorld::new();
        let size = Winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(&size, None).unwrap();
        let command = WindowCommand::Plain(attach("plain", "sleep 30"));
        let argv = world.wrapper_argv(&world.mark, &command);
        let out = Command::new(&argv[0])
            .args(&argv[1..])
            .env("FAKE", world.dir.path().join("fake"))
            .stdin(std::fs::File::from(pty.slave))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        drop(pty.master);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
        assert_eq!(world.read("runs"), "", "it attached at 0x0");
        assert!(
            text(&out.stdout).contains("This terminal has no size"),
            "{}",
            text(&out.stdout)
        );
    }

    /// A hung-up window ends the wrapper at once, also while its attach
    /// runs: it sets no SIGHUP trap, which bash would run only after the
    /// attach ended.
    #[test]
    fn wrapper_ends_at_once_on_sighup() {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
        let world = WrapperWorld::new();
        let command = WindowCommand::Plain(attach("plain", "sleep 30"));
        let argv = world.wrapper_argv(&world.mark, &command);
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .env("FAKE", world.dir.path().join("fake"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while world.read("runs").is_empty() {
            assert!(std::time::Instant::now() < deadline, "the attach never ran");
            std::thread::sleep(Duration::from_millis(20));
        }
        let pid = child.id().to_string();
        Command::new("kill").args(["-HUP", &pid]).status().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if std::time::Instant::now() > deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // The attach's sleep, in the wrapper's group.
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .status();
        let _ = child.wait();
        assert_eq!(status.and_then(|s| s.signal()), Some(1), "{status:?}");
    }

    #[test]
    fn wrapper_closes_quietly_after_a_long_successful_run() {
        let world = WrapperWorld::new();
        let argv = strings(&["sleep", "2"]);
        let out = world.run(&WindowCommand::Plain(argv), "", AT_ONCE);
        assert!(out.status.success());
        assert_eq!(text(&out.stdout), "\x1b]0;i3mux:deepthought:ws4-002\x07");
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

    /// PLAN-migrate decision 9: the wrapper asks reattach after every end
    /// of its attach, and again after ssh recovery, before REATTACH. Here
    /// the session never moves, so the window does what it did before.
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
        let asked = reattach_from("deepthought");
        assert_eq!(
            world.read("runs"),
            format!("plain\n{asked}{asked}force a'b \"$HOME\"\n{asked}")
        );
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
        assert_eq!(world.attaches(), "plain\n", "no reattach without a key");
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
        assert_eq!(world.attaches(), "plain\nforce\n");
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
        assert_eq!(world.attaches(), "plain\n");
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
        assert_eq!(world.attaches(), "plain\nplain\n");
        let text = text(&out.stdout);
        assert!(
            text.contains("[i3mux] Starting a new shell in this window.\n"),
            "{text}"
        );
        assert!(text.contains("with exit status 75."), "{text}");
    }

    #[test]
    fn a_no_hold_window_ends_with_the_shell_and_never_pauses() {
        // PLAN-terminal-here decision 4: a failure or a quick end ends the
        // window at once, with the shell's status, and no hint; stdin stays
        // open, so a pause would hang until it closes.
        let mut world = WrapperWorld::new();
        world.options.no_hold = true;
        for (argv, status) in [
            (strings(&["sh", "-c", "exit 3"]), 3),
            (strings(&["true"]), 0),
        ] {
            let started = std::time::Instant::now();
            let out = world.run(&WindowCommand::Plain(argv), "", Duration::from_secs(5));
            assert_eq!(out.status.code(), Some(status));
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "it waited for Enter"
            );
            assert!(
                !text(&out.stdout).contains("Press Enter"),
                "{}",
                text(&out.stdout)
            );
        }
    }

    #[test]
    fn an_attach_only_window_starts_no_new_shell_after_the_daemon_was_replaced() {
        // The argv attaches an existing shell: running it again after exit
        // 75 would start a new one under its name.
        let mut world = WrapperWorld::new();
        world.options = WrapperOptions {
            no_hold: true,
            attach_only: true,
        };
        world.set_unit(None, "active", "running", "A", 0);
        let command = remote(attach("plain", "exit 75"), attach("force", "exit 0"));
        let out = world.run(&command, "", AT_ONCE);
        assert_eq!(out.status.code(), Some(75));
        // reattach gets the window's options, so a followed window keeps
        // them.
        assert_eq!(
            world.read("runs"),
            "plain\nreattach --no-hold --attach-only --shell ws4-002 --from deepthought\n"
        );
        assert!(!text(&out.stdout).contains("Starting a new shell"));
    }

    #[test]
    fn the_options_come_before_the_window_words() {
        let command = remote(strings(&["ssh", "a"]), strings(&["ssh", "-f"]));
        let options = WrapperOptions {
            no_hold: true,
            attach_only: true,
        };
        assert_eq!(
            fixed_launcher("foot").wrapper_argv(&mark_on("h"), &command, options)[..7],
            strings(&[
                "/w.sh",
                "--no-hold",
                "--attach-only",
                "/bin/i3mux",
                "h",
                "ws4-002",
                "--remote"
            ])
        );
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
        assert_eq!(world.attaches(), "plain\n");
        let stdout = text(&out.stdout);
        assert!(stdout.contains("with exit status 1."), "{stdout}");
        assert!(stdout.contains("'i3mux terminal --force'"), "{stdout}");
        assert!(text(&out.stderr).contains("Shared connection to deepthought closed."));
        assert!(!world.read("calls").contains("restart"));
    }

    // PLAN-migrate decision 9: a window follows its session to another host.

    /// When reattach prints a wrapper argv and exits 0, the window execs
    /// it: no hint for the shell that ended. The new wrapper asks again,
    /// from the new host, when its own attach ends.
    #[test]
    fn a_window_follows_its_session_to_the_wrapper_that_reattach_prints() {
        let world = WrapperWorld::new();
        let next = world.wrapper_argv(
            &mark_on("dev-box"),
            &WindowCommand::Plain(attach("new", "sleep 2")),
        );
        world.answer_reattach(1, 0, &next);

        let out = world.run(&WindowCommand::Plain(attach("old", "exit 0")), "", AT_ONCE);

        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
        assert_eq!(
            world.read("runs"),
            format!(
                "old\n{}new\n{}",
                reattach_from("deepthought"),
                reattach_from("dev-box")
            )
        );
        let stdout = text(&out.stdout);
        assert!(
            stdout.contains("\x1b]0;i3mux:dev-box:ws4-002\x07"),
            "{stdout:?}"
        );
        assert!(!stdout.contains("Press Enter"), "{stdout}");
    }

    /// Rule 3 in the wrapper, after a migration. The old host's ssh exits
    /// 75: the window asks reattach before it would start a new shell
    /// there, and follows. The new host does not answer: its ssh exits 255,
    /// the window asks again, then shows the landing page of the new host
    /// until its link is back, asks once more, and reattaches there. The
    /// old host gets one attach, the one before the move.
    #[test]
    fn after_a_move_a_window_never_attaches_on_its_old_host() {
        let world = WrapperWorld::new();
        // Show 1: the old wrapper. Show 2: the new one, before its attach.
        // Show 3: the new host's link is down. Show 4 on: it is back.
        world.set_unit(Some(1), "active", "running", "A", 0);
        world.set_unit(Some(2), "active", "running", "A", 0);
        world.set_unit(Some(3), "activating", "auto-restart", "A", 0);
        world.set_unit(None, "active", "running", "B", 1);
        let there = remote_on(
            "dev-box",
            attach("new", "exit 255"),
            attach("new-force", "sleep 2"),
        );
        world.answer_reattach(1, 0, &world.wrapper_argv(&mark_on("dev-box"), &there));
        let command = remote(attach("old", "exit 75"), attach("old-force", "exit 0"));

        let out = world.run(&command, "", Duration::from_secs(10));

        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
        let (old, new) = (reattach_from("deepthought"), reattach_from("dev-box"));
        assert_eq!(
            world.read("runs"),
            format!("old\n{old}new\n{new}{new}new-force\n{new}")
        );
        let stdout = text(&out.stdout);
        assert!(
            stdout
                .contains("[i3mux] Connection to dev-box lost. Shell ws4-002 is safe on the host."),
            "{stdout}"
        );
        assert!(!stdout.contains("Starting a new shell"), "{stdout}");
    }

    /// After ssh recovery, a session that moved while the link was down is
    /// followed before REATTACH runs: the old host gets no `--force`
    /// attach.
    #[test]
    fn a_window_follows_a_move_made_while_its_link_was_down() {
        let world = WrapperWorld::new();
        world.set_unit(Some(1), "active", "running", "A", 0);
        world.set_unit(None, "active", "running", "B", 1);
        let next = world.wrapper_argv(
            &mark_on("dev-box"),
            &WindowCommand::Plain(attach("new", "sleep 2")),
        );
        world.answer_reattach(2, 0, &next);
        let command = remote(attach("old", "exit 255"), attach("old-force", "exit 0"));

        let out = world.run(&command, "", AT_ONCE);

        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
        let old = reattach_from("deepthought");
        assert_eq!(
            world.read("runs"),
            format!("old\n{old}{old}new\n{}", reattach_from("dev-box"))
        );
    }

    /// A reattach that exits 0 but prints nothing, or prints an argv but
    /// fails (also with the status 6 that an earlier reattach had), moves
    /// nothing: the window takes its usual path.
    #[test]
    fn a_failed_or_empty_reattach_is_no_move() {
        let moved = strings(&["bash", "-c", "echo moved"]);
        for (status, words) in [(0, vec![]), (1, moved.clone()), (6, moved)] {
            let world = WrapperWorld::new();
            world.answer_reattach(1, status, &words);
            let command = WindowCommand::Plain(strings(&["sh", "-c", "exit 3"]));

            let out = world.run(&command, "\n", AT_ONCE);

            assert_eq!(out.status.code(), Some(3));
            let stdout = text(&out.stdout);
            assert!(stdout.contains("with exit status 3."), "{stdout}");
            assert!(!stdout.contains("moved"), "{stdout}");
        }
    }
}
