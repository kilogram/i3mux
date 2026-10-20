//! Saved sessions: the layout of a detached session, stored on the host that
//! runs its shells, at `~/.local/state/i3mux/sessions/{name}.json`, and the
//! session's token, in the host's runtime directory (see `host`).
//!
//! Guarantees:
//!
//! - A [`SavedSession`] has at least one terminal, and every terminal
//!   belongs to its own session. [`SavedSession::new`] checks this, and so
//!   does every load, so a hand-edited file cannot make `attach` open another
//!   session's shells.
//! - A loaded file's `name` field matches its file name.
//! - [`take`] writes a new token to the host and returns it
//!   (plan-layout-backup rule 1). [`read_token`] only reads it (rule 3).
//!   [`remove_token`] ends the session (rule 1b). A token never comes back:
//!   each take writes 16 new random bytes.
//! - [`compare_tokens`] is the one rule for whether a binding still holds
//!   its session: same token, another token, or no token file (rules 4, 5,
//!   13 and 13a). The checked-write script on the host applies the same
//!   rule for [`save_if_token`].
//! - [`save_if_token`] is the only way to write a layout. It writes only if
//!   the host still has the caller's token, and it compares and writes in one
//!   host script under `flock` (plan-layout-backup rule 4). It replaces the
//!   file atomically (plan rule 7).

use serde::{Deserialize, Serialize};

use crate::host::{CheckedWrite, HostError, HostExec};
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
    #[error("{host} has a bad token for session '{session}': {source}")]
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
        check_layout(&name, &layout)?;
        Ok(Self {
            name,
            host,
            layout,
            saved_at_unix,
        })
    }

    #[must_use]
    pub fn name(&self) -> &SessionName {
        &self.name
    }

    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }
}

/// Check that `layout` can be restored as session `name`: it has at least
/// one terminal, and every terminal belongs to `name`. Saved sessions and
/// local backups both pass this check.
///
/// # Errors
/// [`SessionError::Empty`] if `layout` has no terminals,
/// [`SessionError::ForeignTerminal`] if one belongs to another session.
pub fn check_layout(name: &SessionName, layout: &Layout) -> Result<(), SessionError> {
    let terminals = layout.terminals();
    if terminals.is_empty() {
        return Err(SessionError::Empty {
            session: name.clone(),
        });
    }
    match terminals.into_iter().find(|t| t.session() != name) {
        Some(foreign) => Err(SessionError::ForeignTerminal {
            session: name.clone(),
            terminal: foreign.clone(),
        }),
        None => Ok(()),
    }
}

fn path_of(name: &SessionName) -> String {
    format!("{SESSIONS_DIR}/{name}.json")
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
    let printed = host.take_token(name)?;
    parse_token(host, name, &printed)
}

/// The host's token of session `name`, or `None` if it has none: no take
/// ever happened, or the session ended (rule 1b). Reads without the lock: a
/// take replaces the file by rename, so a read sees the old or the new
/// token. Never writes (plan-layout-backup rule 3).
///
/// # Errors
/// [`SessionError::Host`] if the host fails, [`SessionError::BadToken`] if
/// the file holds something that is not a token (rule 14).
pub fn read_token(host: &dyn HostExec, name: &SessionName) -> Result<Option<Token>, SessionError> {
    host.read_token(name)?
        .map(|text| parse_token(host, name, text.trim_end()))
        .transpose()
}

/// End session `name` on `host`: delete its token file, under the token
/// lock (plan-layout-backup rule 1b). Every binding with a token of this
/// session then finds it ended.
///
/// # Errors
/// [`SessionError::Host`] if the host fails.
pub fn remove_token(host: &dyn HostExec, name: &SessionName) -> Result<(), SessionError> {
    Ok(host.remove_token(name)?)
}

fn parse_token(host: &dyn HostExec, name: &SessionName, text: &str) -> Result<Token, SessionError> {
    Token::parse(text).map_err(|source| SessionError::BadToken {
        host: host.host().clone(),
        session: name.clone(),
        source,
    })
}

/// Whether a binding with token `ours` still holds its session, given the
/// host's token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenMatch {
    /// The host has the binding's token, or neither has one (a binding from
    /// before tokens, plan-layout-backup rule 4).
    Same,
    /// The host has another token: another machine took the session since
    /// (rules 5 and 13).
    TakenElsewhere,
    /// The binding has a token, and the host has no token file: `kill` or a
    /// host reboot ended the session (rules 1a, 1b and 13a).
    Ended,
}

/// Compare the token `ours` of a binding with the host's token `host`.
#[must_use]
pub fn compare_tokens(ours: Option<&Token>, host: Option<&Token>) -> TokenMatch {
    match (ours, host) {
        (Some(_), None) => TokenMatch::Ended,
        (ours, host) if ours == host => TokenMatch::Same,
        _ => TokenMatch::TakenElsewhere,
    }
}

/// Write `session` to its file on `host`, replacing any earlier save, if
/// [`compare_tokens`] with the host's token is [`TokenMatch::Same`]. The
/// compare and the write run in one host script under `flock`
/// (plan-layout-backup rule 4). Returns what the compare found; only
/// [`TokenMatch::Same`] wrote.
///
/// # Errors
/// [`SessionError::Host`] if the host fails; the old file is then unchanged.
pub fn save_if_token(
    host: &dyn HostExec,
    session: &SavedSession,
    token: Option<&Token>,
) -> Result<TokenMatch, SessionError> {
    let json = serde_json::to_string_pretty(session).map_err(|source| SessionError::Encode {
        session: session.name.clone(),
        source,
    })?;
    let written = host.write_file_if_token(&path_of(&session.name), &json, &session.name, token)?;
    Ok(match written {
        CheckedWrite::Written => TokenMatch::Same,
        CheckedWrite::OtherToken => TokenMatch::TakenElsewhere,
        CheckedWrite::NoTokenFile => TokenMatch::Ended,
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
                local_runtime_dir: Some(home.join("run")),
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
            assert_eq!(outcome, TokenMatch::Same);
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
        let file = home.path().join("run/i3mux/sessions/ws4.token");
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
            TokenMatch::TakenElsewhere,
            "a binding without a token does not hold a taken session"
        );
        assert_eq!(load(&host, &name("ws4")).unwrap(), None);
        assert_eq!(
            save_if_token(&host, &saved("ws4"), Some(&mine)).unwrap(),
            TokenMatch::Same
        );

        take(&host, &name("ws4")).unwrap();
        let mut newer = saved("ws4");
        newer.saved_at_unix += 1;
        assert_eq!(
            save_if_token(&host, &newer, Some(&mine)).unwrap(),
            TokenMatch::TakenElsewhere
        );
        assert_eq!(load(&host, &name("ws4")).unwrap(), Some(saved("ws4")));
    }

    #[test]
    fn read_token_reads_what_the_last_take_wrote() {
        // Plan-layout-backup rules 3 and 14.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        assert_eq!(read_token(&host, &name("ws4")).unwrap(), None);
        let taken = take(&host, &name("ws4")).unwrap();
        assert_eq!(
            read_token(&host, &name("ws4")).unwrap(),
            Some(taken.clone())
        );
        assert_eq!(
            read_token(&host, &name("ws4")).unwrap(),
            Some(taken),
            "a read never takes"
        );

        let file = home.path().join("run/i3mux/sessions/ws4.token");
        std::fs::write(&file, "not a token\n").unwrap();
        let err = read_token(&host, &name("ws4")).unwrap_err();
        assert!(matches!(err, SessionError::BadToken { .. }), "{err}");
    }

    #[test]
    fn a_removed_token_ends_the_session() {
        // Plan-layout-backup rules 1b and 5.
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let mine = take(&host, &name("ws4")).unwrap();
        remove_token(&host, &name("ws4")).unwrap();
        assert_eq!(read_token(&host, &name("ws4")).unwrap(), None);
        assert_eq!(
            save_if_token(&host, &saved("ws4"), Some(&mine)).unwrap(),
            TokenMatch::Ended
        );
        assert_eq!(load(&host, &name("ws4")).unwrap(), None);
        assert_eq!(
            save_if_token(&host, &saved("ws4"), None).unwrap(),
            TokenMatch::Same,
            "a binding from before tokens matches no token file"
        );
    }

    #[test]
    fn compare_tokens_tells_same_taken_and_ended_apart() {
        // Plan-layout-backup rules 4, 5, 13 and 13a. The checked-write
        // script on the host applies the same rule; host::tests cover it.
        let a = Token::parse(&"a".repeat(32)).unwrap();
        let b = Token::parse(&"b".repeat(32)).unwrap();
        let cases = [
            (Some(&a), Some(&a), TokenMatch::Same),
            (None, None, TokenMatch::Same),
            (Some(&a), Some(&b), TokenMatch::TakenElsewhere),
            (None, Some(&b), TokenMatch::TakenElsewhere),
            (Some(&a), None, TokenMatch::Ended),
        ];
        for (ours, host, expected) in cases {
            assert_eq!(compare_tokens(ours, host), expected, "{ours:?} {host:?}");
        }
    }

    #[test]
    fn tokens_are_per_session() {
        let home = tempfile::tempdir().unwrap();
        let host = local(home.path());
        let ws4 = take(&host, &name("ws4")).unwrap();
        take(&host, &name("ws7")).unwrap();
        assert_eq!(
            save_if_token(&host, &saved("ws4"), Some(&ws4)).unwrap(),
            TokenMatch::Same
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
