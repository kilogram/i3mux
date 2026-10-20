//! Local backups of the layouts of bound workspaces (plan-layout-backup
//! decision 3), at `~/.local/state/i3mux/backups/HOST/SESSION.json`. Each
//! holds the token of its binding and the layout, in the layout format of a
//! saved session.
//!
//! Guarantees:
//!
//! - [`BackupStore::read`] returns a layout only if the backup holds the
//!   binding's token (rule 9). A missing, unparseable or invalid backup
//!   reads as no backup; the last two print a warning.
//! - Every backup is its own file, apart from `state.json`, so a bad backup
//!   never changes a binding or another backup.
//! - A layout read or written passes [`session::check_layout`], so a restore
//!   opens only shells of the binding's session.
//! - [`BackupStore::write`] replaces the file atomically
//!   ([`crate::state::replace_file`]). Callers hold the state lock for writes and
//!   removals (rule 6).
//! - Nothing here contacts a host (rule 10).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::Layout;
use crate::session;
use crate::state::WorkspaceState;
use crate::types::{Host, SessionName, Token};

/// One backup file.
#[derive(Serialize, Deserialize)]
struct BackupFile {
    /// The token of the binding when the backup was written. `None` for a
    /// binding from before tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<Token>,
    layout: Layout,
}

/// Owns the backup directory.
#[derive(Debug, Clone)]
pub struct BackupStore {
    dir: PathBuf,
}

impl BackupStore {
    /// A store in an explicit directory.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `$XDG_STATE_HOME/i3mux/backups`, by default `~/.local/state/i3mux/backups`.
    ///
    /// # Errors
    /// If no state directory can be found.
    pub fn from_env() -> Result<Self> {
        let dir = dirs::state_dir()
            .context("cannot find the state directory; set XDG_STATE_HOME or HOME")?
            .join("i3mux")
            .join("backups");
        Ok(Self::new(dir))
    }

    fn path_of(&self, host: &Host, session: &SessionName) -> PathBuf {
        self.dir.join(host.label()).join(format!("{session}.json"))
    }

    /// Replace the backup of `binding` with `layout`, under the binding's
    /// token. The caller holds the state lock.
    ///
    /// The watcher (plan-layout-backup phase 3) is the only writer; until
    /// it exists, only tests call this, and the phase 3 change removes the
    /// `cfg(test)`.
    ///
    /// # Errors
    /// If `layout` fails [`session::check_layout`], or the file cannot be
    /// written; the old backup is then unchanged.
    #[cfg(test)]
    pub fn write(&self, binding: &WorkspaceState, layout: &Layout) -> Result<()> {
        session::check_layout(binding.session(), layout)?;
        let file = BackupFile {
            token: binding.token().cloned(),
            layout: layout.clone(),
        };
        let json = serde_json::to_string_pretty(&file).context("cannot encode the backup")?;
        crate::state::replace_file(&self.path_of(binding.host(), binding.session()), &json)
    }

    /// The backed-up layout of `binding`, or `None` if there is no backup
    /// with the binding's token (rule 9).
    ///
    /// # Errors
    /// If the file exists but cannot be read.
    pub fn read(&self, binding: &WorkspaceState) -> Result<Option<Layout>> {
        let path = self.path_of(binding.host(), binding.session());
        let json = match fs::read_to_string(&path) {
            Ok(json) => json,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
        };
        let parsed = serde_json::from_str::<BackupFile>(&json)
            .map_err(|err| err.to_string())
            .and_then(|file| {
                session::check_layout(binding.session(), &file.layout)
                    .map(|()| file)
                    .map_err(|err| err.to_string())
            });
        match parsed {
            Ok(file) if file.token.as_ref() == binding.token() => Ok(Some(file.layout)),
            Ok(_) => Ok(None),
            Err(detail) => {
                eprintln!(
                    "i3mux: warning: {} is not a valid backup ({detail}); ignoring it",
                    path.display()
                );
                Ok(None)
            }
        }
    }

    /// Remove the backup of (`host`, `session`), if there is one
    /// (rule 16). The caller holds the state lock.
    ///
    /// # Errors
    /// If the file exists but cannot be removed.
    pub fn remove(&self, host: &Host, session: &SessionName) -> Result<()> {
        let path = self.path_of(host, session);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("cannot remove {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RemoteHost, SwayInstance, TerminalName};

    fn name(text: &str) -> SessionName {
        SessionName::new(text).unwrap()
    }

    fn deepthought() -> Host {
        Host::Ssh(RemoteHost::new("deepthought").unwrap())
    }

    fn binding(digit: char) -> WorkspaceState {
        let token = Token::parse(&digit.to_string().repeat(32)).unwrap();
        let sway = SwayInstance::new("boot", "/run/user/1000/sway-ipc.sock");
        WorkspaceState::new(deepthought(), name("ws4"), token, sway)
    }

    fn layout(session: &str, numbers: &[u32]) -> Layout {
        let children = numbers
            .iter()
            .map(|n| Layout::Terminal {
                terminal: TerminalName::new(&name(session), *n),
                percent: None,
            })
            .collect();
        Layout::HSplit {
            children,
            percent: None,
        }
    }

    #[test]
    fn backup_round_trips_under_host_and_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackupStore::new(dir.path().to_path_buf());
        assert_eq!(store.read(&binding('a')).unwrap(), None);

        store.write(&binding('a'), &layout("ws4", &[1, 2])).unwrap();

        assert!(dir.path().join("deepthought/ws4.json").exists());
        assert_eq!(
            store.read(&binding('a')).unwrap(),
            Some(layout("ws4", &[1, 2]))
        );
        let names: Vec<_> = fs::read_dir(dir.path().join("deepthought"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["ws4.json"], "no temp file left");
    }

    #[test]
    fn backup_with_another_token_is_ignored() {
        // Plan-layout-backup rule 9.
        let dir = tempfile::tempdir().unwrap();
        let store = BackupStore::new(dir.path().to_path_buf());
        store.write(&binding('a'), &layout("ws4", &[1])).unwrap();
        assert_eq!(store.read(&binding('b')).unwrap(), None);
        let legacy = WorkspaceState::without_token(deepthought(), name("ws4"));
        assert_eq!(store.read(&legacy).unwrap(), None);
    }

    #[test]
    fn backup_of_a_binding_from_before_tokens_has_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackupStore::new(dir.path().to_path_buf());
        let legacy = WorkspaceState::without_token(deepthought(), name("ws4"));
        store.write(&legacy, &layout("ws4", &[1])).unwrap();
        assert_eq!(store.read(&legacy).unwrap(), Some(layout("ws4", &[1])));
        assert_eq!(store.read(&binding('a')).unwrap(), None);
    }

    #[test]
    fn unparseable_or_foreign_backups_read_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackupStore::new(dir.path().to_path_buf());
        let path = dir.path().join("deepthought/ws4.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{").unwrap();
        assert_eq!(store.read(&binding('a')).unwrap(), None);

        let foreign = serde_json::json!({
            "token": "a".repeat(32),
            "layout": layout("ws5", &[1]),
        });
        fs::write(&path, foreign.to_string()).unwrap();
        assert_eq!(store.read(&binding('a')).unwrap(), None);
        assert!(store.write(&binding('a'), &layout("ws5", &[1])).is_err());
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackupStore::new(dir.path().to_path_buf());
        store.write(&binding('a'), &layout("ws4", &[1])).unwrap();
        store.remove(&deepthought(), &name("ws4")).unwrap();
        store.remove(&deepthought(), &name("ws4")).unwrap();
        assert_eq!(store.read(&binding('a')).unwrap(), None);
    }
}
