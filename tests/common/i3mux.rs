//! The `i3mux` command line as the tests run it: inside the Sway container,
//! with the Sway environment loaded, foot as `$TERMINAL`, and a UTF-8
//! locale (without one, foot prints a locale warning into every terminal,
//! and the goldens would show it).

use std::fmt;

use anyhow::{bail, Result};

use super::docker::{Containers, LOCAL_HOME, REMOTE_HOME, REMOTE_HOST};

/// Where a test session's shells run. Every session-level test runs once
/// for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    /// Shells in the Sway container itself.
    Local,
    /// Shells in the sshd container, reached as `--remote i3mux-remote-ssh`.
    Remote,
}

impl Session {
    /// The `--remote` option for this session, with a leading space, or "".
    #[must_use]
    pub fn remote_flag(self) -> String {
        match self {
            Self::Local => String::new(),
            Self::Remote => format!(" --remote {REMOTE_HOST}"),
        }
    }

    /// The host label i3mux prints and puts in window marks.
    #[must_use]
    pub fn host_label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => REMOTE_HOST,
        }
    }

    /// How detach, attach and resume messages name the host: the host, or
    /// "this machine".
    #[must_use]
    pub fn place(self) -> &'static str {
        match self {
            Self::Local => "this machine",
            Self::Remote => REMOTE_HOST,
        }
    }

    /// The home directory of the user that owns the shells.
    #[must_use]
    pub fn home(self) -> &'static str {
        match self {
            Self::Local => LOCAL_HOME,
            Self::Remote => REMOTE_HOME,
        }
    }

    /// The window mark of terminal `number` of `session`.
    #[must_use]
    pub fn mark(self, session: &str, number: u32) -> String {
        format!("_i3mux:{}:{}", self.host_label(), terminal(session, number))
    }
}

/// The shpool session name of terminal `number` of `session`: `ws4-001`.
#[must_use]
pub fn terminal(session: &str, number: u32) -> String {
    format!("{session}-{number:03}")
}

/// What an `i3mux` run printed and how it exited.
#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub command: String,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Fail unless the command exited 0; return its stdout.
    pub fn ok(self) -> Result<String> {
        if !self.success() {
            bail!("{self}");
        }
        Ok(self.stdout)
    }

    /// Fail unless the command exited with `code`; return its stderr.
    pub fn failed_with(self, code: i32) -> Result<String> {
        if self.code != Some(code) {
            bail!("expected exit {code}; got {self}");
        }
        Ok(self.stderr)
    }
}

impl fmt::Display for CmdOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` exited {:?}\nstdout: {}\nstderr: {}",
            self.command,
            self.code,
            self.stdout.trim(),
            self.stderr.trim()
        )
    }
}

/// Runs `i3mux` subcommands in the Sway container.
pub struct I3mux<'a> {
    containers: &'a Containers,
}

impl<'a> I3mux<'a> {
    #[must_use]
    pub fn new(containers: &'a Containers) -> Self {
        Self { containers }
    }

    /// `i3mux ARGS`; `args` is shell text (quote arguments yourself).
    pub fn run(&self, args: &str) -> Result<CmdOutput> {
        let command = format!("i3mux {args}");
        let output = self.containers.exec_in_sway(&format!(
            "source /tmp/sway-env.sh && export TERMINAL=foot LANG=C.UTF-8 && {command}"
        ))?;
        Ok(CmdOutput {
            command,
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// `i3mux activate [--remote H]` on the focused workspace.
    pub fn activate(&self, session: Session) -> Result<String> {
        self.run(&format!("activate{}", session.remote_flag()))?
            .ok()
    }

    /// `i3mux terminal ARGS` on the focused workspace.
    pub fn terminal(&self, args: &str) -> Result<String> {
        self.run(&format!("terminal {args}"))?.ok()
    }

    /// `i3mux detach` on the focused workspace.
    pub fn detach(&self) -> Result<String> {
        self.run("detach")?.ok()
    }

    /// `i3mux attach [--remote H] ARGS`, which may fail.
    pub fn attach(&self, session: Session, args: &str) -> Result<CmdOutput> {
        self.run(&format!("attach{} {args}", session.remote_flag()))
    }

    /// The lines of `i3mux sessions [--remote H]`.
    pub fn sessions(&self, session: Session) -> Result<Vec<String>> {
        let out = self
            .run(&format!("sessions{}", session.remote_flag()))?
            .ok()?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    /// `i3mux kill ARGS`, which may fail. Pass `--remote` in `args` if needed.
    pub fn kill(&self, args: &str) -> Result<CmdOutput> {
        self.run(&format!("kill {args}"))
    }
}
