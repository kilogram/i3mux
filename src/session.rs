//! Saved sessions: the layout of a detached session, stored on the host that
//! runs its shells, at `~/.local/state/i3mux/sessions/{name}.json`, and the
//! session's token, at `~/.local/state/i3mux/sessions/{name}.token`.
//!
//! Guarantees:
//!
//! - A [`SavedSession`] has at least one terminal, and every terminal
//!   belongs to its own session. [`SavedSession::new`] checks this, and so
//!   does every load, so a hand-edited file cannot make `attach` open another
//!   session's shells.
//! - A loaded file's `name` field matches its file name.
//! - [`take`] writes a new token to the host and returns it
//!   (plan-layout-backup rule 1). Nothing here deletes a token file, so a
//!   token never comes back after `kill`.
//! - [`save_if_token`] is the only way to write a layout. It writes only if
//!   the host still has the caller's token, and it compares and writes in one
//!   host script under `flock` (plan-layout-backup rule 4). It replaces the
//!   file atomically (plan rule 7).

use serde::{Deserialize, Serialize};

use crate::host::{HostError, HostExec};
use crate::layout::Layout;
use crate::types::{Host, InvalidToken, SessionName, TerminalName, Token};

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
    #[error("{host} returned a bad token for session '{session}': {source}")]
    BadToken {
        host: Host,
        session: SessionName,
        #[source]
        source: InvalidToken,
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

fn token_path_of(name: &SessionName) -> String {
    format!("{SESSIONS_DIR}/{name}.token")
}

/// Take session `name` on `host`: replace its token with a new one and
/// return it (plan-layout-backup rule 1). A binding that holds an older
/// token no longer holds the session.
///
/// # Errors
/// [`SessionError::Host`] if the host fails, and the token is then
/// unchanged. [`SessionError::BadToken`] if the host printed something that
/// is not a token.
pub fn take(host: &dyn HostExec, name: &SessionName) -> Result<Token, SessionError> {
    let printed = host.take_token(&token_path_of(name))?;
    Token::parse(&printed).map_err(|source| SessionError::BadToken {
        host: host.host().clone(),
        session: name.clone(),
        source,
    })
}

/// What [`save_if_token`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    /// The host had the caller's token: the layout is saved.
    Saved,
    /// A later take changed the host's token: nothing is written.
    TakenElsewhere,
}

/// Write `session` to its file on `host`, replacing any earlier save, if the
/// host's token of the session is `token`. `None` matches a host with no
/// token file: a binding from before tokens (plan-layout-backup rule 4).
///
/// # Errors
/// [`SessionError::Host`] if the host fails; the old file is then unchanged.
pub fn save_if_token(
    host: &dyn HostExec,
    session: &SavedSession,
    token: Option<&Token>,
) -> Result<SaveOutcome, SessionError> {
    let json = serde_json::to_string_pretty(session).map_err(|source| SessionError::Encode {
        session: session.name.clone(),
        source,
    })?;
    let path = path_of(&session.name);
    let token_path = token_path_of(&session.name);
    let written = host.write_file_if_token(&path, &json, &token_path, token)?;
    Ok(if written {
        SaveOutcome::Saved
    } else {
        SaveOutcome::TakenElsewhere
    })
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
                masters: std::sync::Arc::new(crate::master::DirectSsh),
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

        for session in ["ws7", "ws4"] {
            let outcome = save_if_token(&host, &saved(session), None).unwrap();
            assert_eq!(outcome, SaveOutcome::Saved);
        }
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
    fn take_writes_a_new_token_each_time() {
        // Plan-layout-backup rule 1.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let first = take(&host, &name("ws4")).unwrap();
        let second = take(&host, &name("ws4")).unwrap();
        assert_ne!(first, second);
        let file = home.path().join(SESSIONS_DIR).join("ws4.token");
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            format!("{}\n", second.as_str())
        );
        assert_eq!(
            list(&host).unwrap(),
            [],
            "a token alone is no saved session"
        );
    }

    #[test]
    fn save_if_token_saves_only_while_the_host_has_the_token() {
        // Plan-layout-backup rule 4.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let mine = take(&host, &name("ws4")).unwrap();
        assert_eq!(
            save_if_token(&host, &saved("ws4"), None).unwrap(),
            SaveOutcome::TakenElsewhere,
            "a binding without a token does not hold a taken session"
        );
        assert_eq!(load(&host, &name("ws4")).unwrap(), None);
        assert_eq!(
            save_if_token(&host, &saved("ws4"), Some(&mine)).unwrap(),
            SaveOutcome::Saved
        );

        take(&host, &name("ws4")).unwrap();
        let mut newer = saved("ws4");
        newer.saved_at_unix += 1;
        assert_eq!(
            save_if_token(&host, &newer, Some(&mine)).unwrap(),
            SaveOutcome::TakenElsewhere
        );
        assert_eq!(load(&host, &name("ws4")).unwrap(), Some(saved("ws4")));
    }

    #[test]
    fn tokens_are_per_session() {
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let ws4 = take(&host, &name("ws4")).unwrap();
        take(&host, &name("ws7")).unwrap();
        assert_eq!(
            save_if_token(&host, &saved("ws4"), Some(&ws4)).unwrap(),
            SaveOutcome::Saved
        );
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
