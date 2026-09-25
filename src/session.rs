//! Saved sessions: the layout of a detached session, stored on the host that
//! runs its shells, at `~/.local/state/i3mux/sessions/{name}.json`.
//!
//! Guarantees:
//!
//! - A [`SavedSession`] has at least one terminal, and every terminal
//!   belongs to its own session. [`SavedSession::new`] checks this, and so
//!   does every load, so a hand-edited file cannot make `attach` open another
//!   session's shells.
//! - A loaded file's `name` field matches its file name.
//! - [`save`] replaces the file atomically (plan rule 7).

use serde::{Deserialize, Serialize};

use crate::host::{HostError, HostExec};
use crate::layout::Layout;
use crate::types::{Host, SessionName, TerminalName};

/// Directory of saved sessions, relative to the host user's home.
const SESSIONS_DIR: &str = ".local/state/i3mux/sessions";

/// A saved session is invalid, or the host failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("the saved layout of session '{session}' has no terminals")]
    Empty { session: SessionName },
    #[error("the saved layout of session '{session}' refers to terminal '{terminal}' of another session")]
    ForeignTerminal {
        session: SessionName,
        terminal: TerminalName,
    },
    #[error("~/{path} on {host} is not a valid saved session: {detail}")]
    Corrupt {
        host: Host,
        path: String,
        detail: String,
    },
    #[error("cannot encode session '{session}': {source}")]
    Encode {
        session: SessionName,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Host(#[from] HostError),
}

/// The layout of a detached session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "SavedSessionFields")]
pub struct SavedSession {
    name: SessionName,
    /// The host the session was detached from, for display.
    host: Host,
    layout: Layout,
    saved_at_unix: u64,
}

#[derive(Deserialize)]
struct SavedSessionFields {
    name: SessionName,
    host: Host,
    layout: Layout,
    saved_at_unix: u64,
}

impl TryFrom<SavedSessionFields> for SavedSession {
    type Error = SessionError;
    fn try_from(f: SavedSessionFields) -> Result<Self, Self::Error> {
        Self::new(f.name, f.host, f.layout, f.saved_at_unix)
    }
}

impl SavedSession {
    /// A saved session of `name` with `layout`.
    ///
    /// # Errors
    /// [`SessionError::Empty`] if `layout` has no terminals,
    /// [`SessionError::ForeignTerminal`] if one belongs to another session.
    pub fn new(
        name: SessionName,
        host: Host,
        layout: Layout,
        saved_at_unix: u64,
    ) -> Result<Self, SessionError> {
        let terminals = layout.terminals();
        if terminals.is_empty() {
            return Err(SessionError::Empty { session: name });
        }
        if let Some(foreign) = terminals.into_iter().find(|t| t.session() != &name) {
            return Err(SessionError::ForeignTerminal {
                session: name,
                terminal: foreign.clone(),
            });
        }
        Ok(Self {
            name,
            host,
            layout,
            saved_at_unix,
        })
    }

    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }
}

fn path_of(name: &SessionName) -> String {
    format!("{SESSIONS_DIR}/{name}.json")
}

/// Write `session` to its file on `host`, replacing any earlier save.
///
/// # Errors
/// [`SessionError::Host`] if the write fails; the old file is then unchanged.
pub fn save(host: &dyn HostExec, session: &SavedSession) -> Result<(), SessionError> {
    let json = serde_json::to_string_pretty(session).map_err(|source| SessionError::Encode {
        session: session.name.clone(),
        source,
    })?;
    host.write_file_atomic(&path_of(&session.name), &json)?;
    Ok(())
}

/// The saved session `name` on `host`, or `None` if there is none.
///
/// # Errors
/// [`SessionError::Corrupt`] if the file is not a valid saved session of
/// that name; [`SessionError::Host`] if the host fails.
pub fn load(host: &dyn HostExec, name: &SessionName) -> Result<Option<SavedSession>, SessionError> {
    let path = path_of(name);
    let Some(json) = host.read_file(&path)? else {
        return Ok(None);
    };
    let corrupt = |detail: String| SessionError::Corrupt {
        host: host.host().clone(),
        path: path.clone(),
        detail,
    };
    let session: SavedSession = serde_json::from_str(&json).map_err(|e| corrupt(e.to_string()))?;
    if &session.name != name {
        return Err(corrupt(format!("it names session '{}'", session.name)));
    }
    Ok(Some(session))
}

/// Names of the saved sessions on `host`, sorted.
///
/// # Errors
/// [`SessionError::Host`] if the host fails.
pub fn list(host: &dyn HostExec) -> Result<Vec<SessionName>, SessionError> {
    let mut names: Vec<SessionName> = host
        .list_dir(SESSIONS_DIR)?
        .iter()
        .filter_map(|file| SessionName::new(file.strip_suffix(".json")?).ok())
        .collect();
    names.sort();
    Ok(names)
}

/// Delete the saved session `name`. Returns whether it existed.
///
/// # Errors
/// [`SessionError::Host`] if the host fails.
pub fn delete(host: &dyn HostExec, name: &SessionName) -> Result<bool, SessionError> {
    Ok(host.remove_file(&path_of(name))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{HostRunner, RunnerConfig};
    use crate::types::RemoteHost;

    fn local(home: &std::path::Path) -> HostRunner {
        HostRunner::new(
            Host::Local,
            RunnerConfig {
                local_home: home.to_path_buf(),
                ssh_control_dir: home.join("ssh"),
            },
        )
    }

    fn name(text: &str) -> SessionName {
        SessionName::new(text).unwrap()
    }

    fn two_terminals(session: &str) -> Layout {
        let s = name(session);
        Layout::HSplit {
            children: vec![
                Layout::Terminal {
                    terminal: TerminalName::new(&s, 1),
                    percent: None,
                },
                Layout::Terminal {
                    terminal: TerminalName::new(&s, 2),
                    percent: None,
                },
            ],
            percent: None,
        }
    }

    fn saved(session: &str) -> SavedSession {
        let host = Host::Ssh(RemoteHost::new("deepthought").unwrap());
        SavedSession::new(name(session), host, two_terminals(session), 1_790_000_000).unwrap()
    }

    #[test]
    fn save_load_list_delete_round_trip() {
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        assert_eq!(list(&host).unwrap(), []);
        assert_eq!(load(&host, &name("ws4")).unwrap(), None);

        save(&host, &saved("ws7")).unwrap();
        save(&host, &saved("ws4")).unwrap();
        assert!(home
            .path()
            .join(".local/state/i3mux/sessions/ws4.json")
            .exists());
        assert_eq!(list(&host).unwrap(), [name("ws4"), name("ws7")]);
        assert_eq!(load(&host, &name("ws4")).unwrap(), Some(saved("ws4")));

        assert!(delete(&host, &name("ws4")).unwrap());
        assert!(!delete(&host, &name("ws4")).unwrap());
        assert_eq!(list(&host).unwrap(), [name("ws7")]);
    }

    #[test]
    fn saved_session_rejects_foreign_or_missing_terminals() {
        let err =
            SavedSession::new(name("ws4"), Host::Local, two_terminals("ws4-x"), 0).unwrap_err();
        assert!(matches!(err, SessionError::ForeignTerminal { .. }), "{err}");
        let empty = Layout::Tabbed { children: vec![] };
        let err = SavedSession::new(name("ws4"), Host::Local, empty, 0).unwrap_err();
        assert!(matches!(err, SessionError::Empty { .. }), "{err}");
    }

    #[test]
    fn load_rejects_hand_edited_files() {
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let dir = home.path().join(SESSIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();

        let mut foreign = serde_json::to_value(saved("ws4")).unwrap();
        foreign["layout"]["children"][0]["socket"] = "ws5-001".into();
        std::fs::write(dir.join("ws4.json"), foreign.to_string()).unwrap();
        assert!(matches!(
            load(&host, &name("ws4")),
            Err(SessionError::Corrupt { .. })
        ));

        std::fs::write(
            dir.join("ws4.json"),
            serde_json::to_string(&saved("ws9")).unwrap(),
        )
        .unwrap();
        let err = load(&host, &name("ws4")).unwrap_err();
        assert!(err.to_string().contains("it names session 'ws9'"), "{err}");

        std::fs::write(dir.join("ws4.json"), "{").unwrap();
        assert!(matches!(
            load(&host, &name("ws4")),
            Err(SessionError::Corrupt { .. })
        ));
    }

    #[test]
    fn list_skips_files_that_are_not_sessions() {
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let dir = home.path().join(SESSIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        for file in [
            "ws4.json",
            "notes.txt",
            "bad name.json",
            ".ws5.json.tmp1234",
        ] {
            std::fs::write(dir.join(file), "{}").unwrap();
        }
        assert_eq!(list(&host).unwrap(), [name("ws4")]);
    }
}
