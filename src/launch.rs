//! Launch terminal emulator windows.
//!
//! An i3mux window runs `wrapper.sh TITLE ARGV...` in `$TERMINAL`. The
//! wrapper sets the title once, runs ARGV with no shell, and keeps the
//! window open if ARGV fails (plan decisions 10 and 13).
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

/// The wrapper script, written to the private runtime directory.
pub const WRAPPER: &str = include_str!("wrapper.sh");
const WRAPPER_FILE: &str = "wrapper.sh";

/// Opens terminal windows. [`TerminalLauncher`] is the real one; tests use
/// fakes that add windows to a fake Sway.
pub trait Launcher {
    /// Start a window with `app_id` and `title` that runs `argv` through the
    /// wrapper. Returns once the terminal process has started.
    ///
    /// # Errors
    /// If the terminal cannot be started.
    fn spawn(&self, app_id: &str, title: &str, argv: &[String]) -> Result<()>;
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
    fn spawn(&self, app_id: &str, title: &str, argv: &[String]) -> Result<()> {
        let mut command = vec![self.wrapper.display().to_string(), title.to_owned()];
        command.extend_from_slice(argv);
        spawn_detached(&window_argv(
            &self.program,
            Some(app_id),
            Some(title),
            &command,
        ))
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

    fn run_wrapper(dir: &Path, argv: &[&str], stdin: &str) -> std::process::Output {
        let launcher = TerminalLauncher::install("foot".into(), dir).unwrap();
        // Run it with sh rather than exec it: this multi-threaded test
        // process may have leaked the write descriptor into another test's
        // fork, which would make exec fail with ETXTBSY.
        let mut child = Command::new("sh")
            .arg(&launcher.wrapper)
            .args(argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn wrapper_sets_title_and_runs_argv_unparsed() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_wrapper(
            dir.path(),
            &["my title", "printf", "%s", "a'b \"$HOME\""],
            "",
        );
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(
            text.starts_with("\x1b]0;my title\x07a'b \"$HOME\"\n"),
            "{text:?}"
        );
    }

    #[test]
    fn wrapper_pauses_with_hint_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_wrapper(dir.path(), &["t", "sh", "-c", "exit 3"], "\n");
        assert_eq!(out.status.code(), Some(3));
        let text = String::from_utf8(out.stdout).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let out = run_wrapper(dir.path(), &["t", "true"], "\n");
        assert_eq!(out.status.code(), Some(0));
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(
            text.contains("already attached from another terminal or host"),
            "{text}"
        );
        assert!(text.contains("Press Enter"), "{text}");
    }

    #[test]
    fn wrapper_closes_quietly_after_a_long_successful_run() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_wrapper(dir.path(), &["t", "sleep", "2"], "");
        assert!(out.status.success());
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "\x1b]0;t\x07");
    }
}
