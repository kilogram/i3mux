//! Start the layout watcher (`i3mux watch`) of the current Sway as a
//! transient systemd user unit (plan-layout-backup rules 17 and 20).
//!
//! Guarantees:
//!
//! - The unit of a Sway is `i3mux-watch-<id>.service`. The id is a fixed
//!   hash (64-bit FNV-1a) of Sway's socket path, so every i3mux command in
//!   one Sway names the same unit, and two Sways never share one. Sway's pid
//!   is not available cleanly: only `SO_PEERCRED` on its socket gives it,
//!   and that needs unsafe code or an unstable std API.
//! - A unit that runs already counts as started
//!   ([`master::start_transient_unit`]).
//! - The unit restarts on failure, and gets `SWAYSOCK`, `I3MUX_STATE_PATH`
//!   and `I3MUX_BACKUP_DIR` from the i3mux that starts it, so it watches
//!   that Sway and uses the same files, whatever environment the user
//!   manager has.
//! - Without a systemd user manager, [`NoWatcher`] starts nothing and the
//!   workspaces have no backup (rule 20).

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{bail, Result};

use crate::master::{self, UnitStart};

/// systemd properties of the watcher unit. It stops with the graphical
/// session, as the ssh masters do, and on its own when Sway exits.
const UNIT_PROPERTIES: [&str; 4] = [
    "Restart=on-failure",
    "RestartSec=2s",
    "PartOf=graphical-session.target",
    "After=graphical-session.target",
];

/// Starts the layout watcher of the current Sway. [`SystemdWatcher`] and
/// [`NoWatcher`] are the real ones; tests use a fake.
pub trait Watcher {
    /// Make sure the watcher of the current Sway runs. One that runs
    /// already is success.
    ///
    /// # Errors
    /// If systemd refuses to start it.
    fn ensure_running(&self) -> Result<()>;
}

/// Runs the watcher as a transient user unit.
#[derive(Debug, Clone)]
pub struct SystemdWatcher {
    argv: Vec<String>,
}

impl SystemdWatcher {
    /// A watcher that `systemd-run` starts with `argv` (see [`unit_argv`]).
    #[must_use]
    pub fn new(argv: Vec<String>) -> Self {
        Self { argv }
    }
}

impl Watcher for SystemdWatcher {
    fn ensure_running(&self) -> Result<()> {
        match master::start_transient_unit(&self.argv)? {
            UnitStart::Running => Ok(()),
            UnitStart::Refused { stderr } => {
                bail!("systemd did not start the layout watcher: {stderr}")
            }
        }
    }
}

/// Stands in for [`SystemdWatcher`] on a machine with no systemd user
/// manager: there is no watcher (rule 20).
#[derive(Debug, Clone, Copy)]
pub struct NoWatcher;

impl Watcher for NoWatcher {
    fn ensure_running(&self) -> Result<()> {
        Ok(())
    }
}

/// The unit of the watcher of the Sway at `sway_socket`.
#[must_use]
pub fn unit_name(sway_socket: &Path) -> String {
    format!(
        "i3mux-watch-{:016x}.service",
        fnv1a(sway_socket.as_os_str().as_bytes())
    )
}

/// 64-bit FNV-1a: a fixed hash, unlike std's, so the unit name of a Sway
/// does not change between i3mux builds.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

/// `systemd-run` that starts `exe watch` as the watcher of the Sway at
/// `sway_socket`, with the state file and backup directory of the caller.
#[must_use]
pub fn unit_argv(
    exe: &Path,
    sway_socket: &Path,
    state_path: &Path,
    backup_dir: &Path,
) -> Vec<String> {
    let mut argv: Vec<String> = ["systemd-run", "--user", "--quiet", "--collect"]
        .map(str::to_owned)
        .to_vec();
    argv.push(format!("--unit={}", unit_name(sway_socket)));
    argv.push(format!(
        "--description=i3mux layout backups for Sway at {}",
        sway_socket.display()
    ));
    for property in UNIT_PROPERTIES {
        argv.extend(["-p".to_owned(), property.to_owned()]);
    }
    for (variable, value) in [
        ("SWAYSOCK", sway_socket),
        ("I3MUX_STATE_PATH", state_path),
        ("I3MUX_BACKUP_DIR", backup_dir),
    ] {
        argv.push(format!("--setenv={variable}={}", value.display()));
    }
    argv.extend([
        "--".to_owned(),
        exe.display().to_string(),
        "watch".to_owned(),
    ]);
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOCKET: &str = "/run/user/1000/sway-ipc.1000.1234.sock";

    #[test]
    fn the_unit_name_is_fixed_for_a_socket() {
        // Pinned: a new name after an upgrade would start a second watcher
        // beside the old one.
        assert_eq!(
            unit_name(Path::new(SOCKET)),
            "i3mux-watch-7099498549655ad4.service"
        );
        assert_ne!(
            unit_name(Path::new(SOCKET)),
            unit_name(Path::new("/run/user/1000/sway-ipc.1000.1235.sock"))
        );
    }

    #[test]
    fn the_unit_restarts_on_failure_and_watches_the_callers_sway_and_files() {
        // Rule 17.
        let argv = unit_argv(
            Path::new("/usr/bin/i3mux"),
            Path::new(SOCKET),
            Path::new("/home/me/.config/i3mux/state.json"),
            Path::new("/home/me/.local/state/i3mux/backups"),
        );
        assert_eq!(argv[..4], ["systemd-run", "--user", "--quiet", "--collect"]);
        assert!(argv.contains(&"--unit=i3mux-watch-7099498549655ad4.service".to_owned()));
        let properties: Vec<&str> = argv
            .windows(2)
            .filter(|w| w[0] == "-p")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(properties, UNIT_PROPERTIES);
        assert!(properties.contains(&"Restart=on-failure"));
        for setenv in [
            format!("--setenv=SWAYSOCK={SOCKET}"),
            "--setenv=I3MUX_STATE_PATH=/home/me/.config/i3mux/state.json".to_owned(),
            "--setenv=I3MUX_BACKUP_DIR=/home/me/.local/state/i3mux/backups".to_owned(),
        ] {
            assert!(argv.contains(&setenv), "{setenv} in {argv:?}");
        }
        assert_eq!(argv[argv.len() - 3..], ["--", "/usr/bin/i3mux", "watch"]);
    }

    #[test]
    fn without_a_user_manager_there_is_no_watcher() {
        // Rule 20.
        NoWatcher.ensure_running().unwrap();
    }
}
