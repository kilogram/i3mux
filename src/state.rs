//! Local state: which Sway workspace is bound to which session.
//!
//! State transitions are pure methods on [`LocalState`]; persistence lives in
//! [`StateStore`], so both are testable against a temporary directory.
//!
//! Guarantees:
//!
//! 1. **One binding each way** (plan rule 1): a workspace binds to at most
//!    one session, and a (host, session) pair to at most one workspace.
//!    [`LocalState::bind`] enforces both.
//! 2. **Atomic writes**: a save fully replaces `state.json` or leaves the old
//!    file; it is never seen half-written.
//! 3. **Corruption tolerance**: an unparseable `state.json` (including the
//!    pre-shpool format) reads as empty. Under the lock it is moved aside to
//!    `state.json.corrupt`; an unlocked read never moves it.
//! 4. **Serialised read-modify-write**: [`StateStore::lock`] holds an
//!    exclusive `flock` from load to drop, across processes and threads.
//!    Commands hold it for their whole run, so concurrent `terminal`,
//!    `attach`, `detach` and `kill` never interleave (plan rule 4).
//! 5. **A pending cleanup is never lost** (PLAN-migrate decision 8): a
//!    binding whose session left a host keeps that host in its
//!    [`PendingCleanup`] until [`LocalState::finish_cleanup`]. It never
//!    names the binding's own host (a load checks this too), a migration
//!    never replaces it with another, and [`LocalState::unbind`] and
//!    [`LocalState::unbind_session`] refuse a binding that has one.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::types::{Host, SessionName, WindowTemplate};

/// The host that a migration moved a session away from, whose shells and
/// saved layout of the session must still end (PLAN-migrate decisions 5
/// and 6). Only [`LocalState::migrate`] makes one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCleanup {
    host: Host,
}

impl PendingCleanup {
    /// The host the session left.
    #[must_use]
    pub fn host(&self) -> &Host {
        &self.host
    }
}

/// What a workspace is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WorkspaceStateFields")]
pub struct WorkspaceState {
    host: Host,
    session: SessionName,
    /// What each window of the workspace runs instead of i3mux's wrapper
    /// (PLAN-terminal-here decision 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window_command: Option<WindowTemplate>,
    /// The host a migration left, until its cleanup ends (guarantee 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cleanup: Option<PendingCleanup>,
}

#[derive(Deserialize)]
struct WorkspaceStateFields {
    host: Host,
    session: SessionName,
    #[serde(default)]
    window_command: Option<WindowTemplate>,
    #[serde(default)]
    cleanup: Option<PendingCleanup>,
}

/// A binding in `state.json` whose pending cleanup names its own host:
/// the cleanup would end the session's live shells.
#[derive(Debug, thiserror::Error)]
#[error("session '{session}' on {host} has a pending cleanup of its own host")]
pub struct CleanupOfOwnHost {
    host: Host,
    session: SessionName,
}

impl TryFrom<WorkspaceStateFields> for WorkspaceState {
    type Error = CleanupOfOwnHost;
    fn try_from(f: WorkspaceStateFields) -> Result<Self, Self::Error> {
        if f.cleanup.as_ref().is_some_and(|c| c.host == f.host) {
            return Err(CleanupOfOwnHost {
                host: f.host,
                session: f.session,
            });
        }
        Ok(Self {
            host: f.host,
            session: f.session,
            window_command: f.window_command,
            cleanup: f.cleanup,
        })
    }
}

impl WorkspaceState {
    #[must_use]
    pub fn new(host: Host, session: SessionName) -> Self {
        Self {
            host,
            session,
            window_command: None,
            cleanup: None,
        }
    }

    /// The host this binding's session left, if its cleanup has not ended.
    #[must_use]
    pub fn cleanup(&self) -> Option<&PendingCleanup> {
        self.cleanup.as_ref()
    }

    /// Whether `other` binds the same session on the same host, with the
    /// same window command. A pending cleanup is the binding's history,
    /// not what it binds.
    fn binds_as(&self, other: &Self) -> bool {
        self.host == other.host
            && self.session == other.session
            && self.window_command == other.window_command
    }

    /// This binding, with `window_command` as its window command.
    #[must_use]
    pub fn with_window_command(mut self, window_command: Option<WindowTemplate>) -> Self {
        self.window_command = window_command;
        self
    }

    /// The window command, if the binding has one.
    #[must_use]
    pub fn window_command(&self) -> Option<&WindowTemplate> {
        self.window_command.as_ref()
    }

    #[must_use]
    pub fn host(&self) -> &Host {
        &self.host
    }

    #[must_use]
    pub fn session(&self) -> &SessionName {
        &self.session
    }
}

/// A bind would break guarantee 1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    #[error("session '{session}' on {host} is already bound to workspace {workspace}")]
    SessionBoundElsewhere {
        host: Host,
        session: SessionName,
        workspace: String,
    },
    #[error(
        "workspace {workspace} is bound to session '{session}' on {host}; \
         use another workspace, or end that session with 'i3mux kill'"
    )]
    WorkspaceTaken {
        workspace: String,
        host: Host,
        session: SessionName,
    },
    #[error("workspace {0} is not bound to a session")]
    NotBound(String),
    #[error(
        "the move of session '{session}' from {left} is not finished: its shells there \
         have not ended"
    )]
    CleanupPending {
        workspace: String,
        session: SessionName,
        /// The host of the pending cleanup.
        left: Host,
    },
}

/// All bindings, keyed by workspace name, as persisted to `state.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalState {
    workspaces: BTreeMap<String, WorkspaceState>,
}

impl LocalState {
    /// Bind workspace `ws` to `binding`. Binding a workspace again to the
    /// same session, host and window command is a no-op, which keeps its
    /// pending cleanup.
    ///
    /// # Errors
    /// [`BindError::SessionBoundElsewhere`] if another workspace has this
    /// (host, session); [`BindError::WorkspaceTaken`] if `ws` is bound to a
    /// different session.
    pub fn bind(&mut self, ws: &str, binding: WorkspaceState) -> Result<(), BindError> {
        if let Some(other) = self.workspace_of(&binding.host, &binding.session) {
            if other != ws {
                return Err(BindError::SessionBoundElsewhere {
                    host: binding.host,
                    session: binding.session,
                    workspace: other.to_owned(),
                });
            }
        }
        match self.workspaces.get(ws) {
            Some(current) if current.binds_as(&binding) => Ok(()),
            Some(current) => Err(BindError::WorkspaceTaken {
                workspace: ws.to_owned(),
                host: current.host.clone(),
                session: current.session.clone(),
            }),
            None => {
                self.workspaces.insert(ws.to_owned(), binding);
                Ok(())
            }
        }
    }

    /// Move the binding of `ws` to host `to`, and record the host it
    /// leaves as its pending cleanup (PLAN-migrate decision 5). Its session
    /// and window command stay. A pending cleanup of `to` is cancelled: the
    /// session goes back to the host whose shells still run (decision 2).
    /// Moving a binding to its own host changes nothing.
    ///
    /// # Errors
    /// [`BindError::NotBound`] for an unbound `ws`;
    /// [`BindError::CleanupPending`] if a cleanup of a third host is
    /// pending, which the move would lose; [`BindError::SessionBoundElsewhere`]
    /// if another workspace has this session on `to`.
    pub fn migrate(&mut self, ws: &str, to: &Host) -> Result<(), BindError> {
        let current = self
            .workspaces
            .get(ws)
            .ok_or_else(|| BindError::NotBound(ws.to_owned()))?;
        if &current.host == to {
            return Ok(());
        }
        if let Some(cleanup) = current.cleanup.as_ref().filter(|c| &c.host != to) {
            return Err(BindError::CleanupPending {
                workspace: ws.to_owned(),
                session: current.session.clone(),
                left: cleanup.host.clone(),
            });
        }
        if let Some(other) = self.workspace_of(to, &current.session) {
            return Err(BindError::SessionBoundElsewhere {
                host: to.clone(),
                session: current.session.clone(),
                workspace: other.to_owned(),
            });
        }
        if let Some(binding) = self.workspaces.get_mut(ws) {
            let left = std::mem::replace(&mut binding.host, to.clone());
            binding.cleanup = Some(PendingCleanup { host: left });
        }
        Ok(())
    }

    /// End the pending cleanup of `ws`, once the session's shells and saved
    /// layout on that host are gone (PLAN-migrate decision 6). Returns it.
    pub fn finish_cleanup(&mut self, ws: &str) -> Option<PendingCleanup> {
        self.workspaces.get_mut(ws)?.cleanup.take()
    }

    /// Remove the binding of `ws`, returning it.
    ///
    /// # Errors
    /// [`BindError::CleanupPending`] if it has a pending cleanup: run the
    /// cleanup first (guarantee 5).
    pub fn unbind(&mut self, ws: &str) -> Result<Option<WorkspaceState>, BindError> {
        if let Some(current) = self.workspaces.get(ws) {
            refuse_pending(ws, current)?;
        }
        Ok(self.workspaces.remove(ws))
    }

    /// Remove the binding of (`host`, `session`), returning its workspace.
    ///
    /// # Errors
    /// As [`LocalState::unbind`].
    pub fn unbind_session(
        &mut self,
        host: &Host,
        session: &SessionName,
    ) -> Result<Option<String>, BindError> {
        let Some(ws) = self.workspace_of(host, session).map(str::to_owned) else {
            return Ok(None);
        };
        self.unbind(&ws)?;
        Ok(Some(ws))
    }

    /// What `ws` is bound to.
    #[must_use]
    pub fn get(&self, ws: &str) -> Option<&WorkspaceState> {
        self.workspaces.get(ws)
    }

    /// The workspace bound to (`host`, `session`).
    #[must_use]
    pub fn workspace_of(&self, host: &Host, session: &SessionName) -> Option<&str> {
        self.workspaces
            .iter()
            .find(|(_, b)| &b.host == host && &b.session == session)
            .map(|(ws, _)| ws.as_str())
    }

    /// Every (workspace, binding), ordered by workspace name.
    pub fn bindings(&self) -> impl Iterator<Item = (&str, &WorkspaceState)> {
        self.workspaces.iter().map(|(ws, b)| (ws.as_str(), b))
    }
}

/// [`BindError::CleanupPending`] if `binding` of `ws` has a pending cleanup.
fn refuse_pending(ws: &str, binding: &WorkspaceState) -> Result<(), BindError> {
    match &binding.cleanup {
        Some(cleanup) => Err(BindError::CleanupPending {
            workspace: ws.to_owned(),
            session: binding.session.clone(),
            left: cleanup.host.clone(),
        }),
        None => Ok(()),
    }
}

/// What [`StateStore::read`] found.
enum Read {
    Valid(LocalState),
    Corrupt(serde_json::Error),
}

/// Owns `state.json`: where it lives, how it is read and written safely,
/// and how concurrent users are serialised.
#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    /// A store at an explicit path.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// `$I3MUX_STATE_PATH` if set, else `$XDG_CONFIG_HOME/i3mux/state.json`.
    ///
    /// # Errors
    /// If no config directory can be found.
    pub fn from_env() -> Result<Self> {
        if let Some(p) = std::env::var_os("I3MUX_STATE_PATH") {
            return Ok(Self::new(PathBuf::from(p)));
        }
        let path = dirs::config_dir()
            .context("cannot find the config directory; set XDG_CONFIG_HOME or HOME")?
            .join("i3mux")
            .join("state.json");
        Ok(Self::new(path))
    }

    /// Read the state without locking, for read-only commands.
    ///
    /// A missing or empty file is the empty state. An unparseable file is
    /// also read as empty, but left in place: only [`StateStore::lock`]
    /// moves it aside, so a reader never races a writer's rename.
    ///
    /// # Errors
    /// If the file exists but cannot be read.
    pub fn load(&self) -> Result<LocalState> {
        Ok(match self.read()? {
            Read::Valid(state) => state,
            Read::Corrupt(err) => {
                eprintln!(
                    "i3mux: warning: {} is not valid ({err}); reading it as empty",
                    self.path.display()
                );
                LocalState::default()
            }
        })
    }

    /// Load under the lock. An unparseable file is moved to
    /// `state.json.corrupt` and the empty state is returned.
    fn load_locked(&self) -> Result<LocalState> {
        Ok(match self.read()? {
            Read::Valid(state) => state,
            Read::Corrupt(err) => {
                let quarantine = self.path.with_extension("json.corrupt");
                eprintln!(
                    "i3mux: warning: {} is not valid ({err}); moving it to {} and starting empty",
                    self.path.display(),
                    quarantine.display()
                );
                if let Err(rename_err) = fs::rename(&self.path, &quarantine) {
                    eprintln!("i3mux: warning: cannot move it aside: {rename_err}");
                }
                LocalState::default()
            }
        })
    }

    fn read(&self) -> Result<Read> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Read::Valid(LocalState::default()))
            }
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", self.path.display()))
            }
        };
        if contents.trim().is_empty() {
            return Ok(Read::Valid(LocalState::default()));
        }
        Ok(match serde_json::from_str(&contents) {
            Ok(state) => Read::Valid(state),
            Err(err) => Read::Corrupt(err),
        })
    }

    /// Replace the file atomically: write a sibling temp file, fsync it,
    /// rename it over the target. A failed save leaves the old file intact.
    ///
    /// # Errors
    /// If the directory or temp file cannot be written.
    pub fn save(&self, state: &LocalState) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let contents = serde_json::to_string_pretty(state).context("cannot encode the state")?;
        {
            let mut file = fs::File::create(&tmp)
                .with_context(|| format!("cannot create {}", tmp.display()))?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("cannot replace {}", self.path.display()))
    }

    /// Take the exclusive lock and load the state. The lock is held until
    /// the returned value drops; changes are written only by
    /// [`StateLock::commit`].
    ///
    /// # Errors
    /// If the lock file cannot be opened or locked, or the state cannot be read.
    pub fn lock(&self) -> Result<StateLock<'_>> {
        let lock_path = self.path.with_extension("json.lock");
        if let Some(dir) = lock_path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("cannot open {}", lock_path.display()))?;
        // Each call opens its own file description, so the lock excludes
        // other threads of this process as well as other processes. The
        // descriptor is close-on-exec: spawned terminals never inherit it.
        file.lock()
            .with_context(|| format!("cannot lock {}", lock_path.display()))?;
        let state = self.load_locked()?;
        Ok(StateLock {
            store: self,
            committed: state.clone(),
            state,
            _lock: file,
        })
    }

    /// Run `f` on the state under the lock and write the result if it
    /// changed. On error nothing is written.
    ///
    /// # Errors
    /// Errors of [`StateStore::lock`], of `f`, or of the save.
    pub fn with_lock<T>(&self, f: impl FnOnce(&mut LocalState) -> Result<T>) -> Result<T> {
        let mut lock = self.lock()?;
        let out = f(lock.state_mut())?;
        lock.commit()?;
        Ok(out)
    }
}

/// The state, loaded under the exclusive lock. Dropping it releases the lock
/// and discards uncommitted changes.
#[derive(Debug)]
pub struct StateLock<'a> {
    store: &'a StateStore,
    state: LocalState,
    committed: LocalState,
    _lock: fs::File,
}

impl StateLock<'_> {
    #[must_use]
    pub fn state(&self) -> &LocalState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut LocalState {
        &mut self.state
    }

    /// Write the state if it changed since the last commit. The lock stays held.
    ///
    /// # Errors
    /// If the save fails; the file then keeps its previous content.
    pub fn commit(&mut self) -> Result<()> {
        if self.state != self.committed {
            self.store.save(&self.state)?;
            self.committed = self.state.clone();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RemoteHost;
    use std::path::Path;
    use std::sync::{Arc, Barrier};

    fn store_in(dir: &Path) -> StateStore {
        StateStore::new(dir.join("state.json"))
    }

    fn remote() -> Host {
        Host::Ssh(RemoteHost::new("user@host").unwrap())
    }

    fn binding(host: Host, session: &str) -> WorkspaceState {
        WorkspaceState::new(host, SessionName::new(session).unwrap())
    }

    fn sample_state(ws: &str) -> LocalState {
        let mut state = LocalState::default();
        state.bind(ws, binding(remote(), "mysession")).unwrap();
        state
    }

    // ---------- transitions (pure) ----------

    #[test]
    fn bind_rejects_duplicate_session() {
        // Plan rule 1: one (host, session) binds to at most one workspace.
        let mut state = LocalState::default();
        state.bind("4", binding(remote(), "ws4")).unwrap();
        let err = state.bind("5", binding(remote(), "ws4")).unwrap_err();
        assert!(
            matches!(&err, BindError::SessionBoundElsewhere { workspace, .. } if workspace == "4")
        );
        assert_eq!(
            err.to_string(),
            "session 'ws4' on user@host is already bound to workspace 4"
        );
        assert_eq!(state.get("5"), None);
        // The same name on another host is another session.
        state.bind("5", binding(Host::Local, "ws4")).unwrap();
    }

    #[test]
    fn bind_rejects_rebinding_a_workspace_to_another_session() {
        let mut state = LocalState::default();
        state.bind("4", binding(Host::Local, "a")).unwrap();
        let err = state.bind("4", binding(Host::Local, "b")).unwrap_err();
        assert!(matches!(err, BindError::WorkspaceTaken { .. }));
        assert_eq!(state.get("4"), Some(&binding(Host::Local, "a")));
    }

    #[test]
    fn bind_is_idempotent() {
        let mut state = sample_state("3");
        let before = state.clone();
        state.bind("3", binding(remote(), "mysession")).unwrap();
        assert_eq!(state, before);
    }

    fn third() -> Host {
        Host::Ssh(RemoteHost::new("third").unwrap())
    }

    /// The pending cleanup of workspace `ws`, by its host.
    fn cleanup_of(state: &LocalState, ws: &str) -> Option<Host> {
        state.get(ws)?.cleanup().map(|c| c.host().clone())
    }

    /// PLAN-migrate decision 5: a migration moves the host and records the
    /// host it leaves; the session and the window command stay. A move to
    /// its own host changes nothing.
    #[test]
    fn migrate_moves_the_host_and_records_the_host_it_leaves() {
        let command = crate::types::WindowTemplate::new("w {shell}").unwrap();
        let mut state = LocalState::default();
        let ws4 = binding(Host::Local, "ws4").with_window_command(Some(command.clone()));
        state.bind("4", ws4).unwrap();

        state.migrate("4", &remote()).unwrap();

        let moved = state.get("4").unwrap();
        assert!(moved.binds_as(&binding(remote(), "ws4").with_window_command(Some(command))));
        assert_eq!(cleanup_of(&state, "4"), Some(Host::Local));
        let same = state.clone();
        state.migrate("4", &remote()).unwrap();
        assert_eq!(state, same);
        assert_eq!(state.finish_cleanup("4").map(|c| c.host), Some(Host::Local));
        assert_eq!(cleanup_of(&state, "4"), None);
        assert_eq!(state.finish_cleanup("4"), None);
    }

    /// PLAN-migrate decision 2: a move back to the host of the pending
    /// cleanup cancels it, and the new cleanup names the host the session
    /// leaves. A move to a third host would lose the cleanup: refused, with
    /// nothing changed.
    #[test]
    fn migrate_back_cancels_the_cleanup_and_a_third_host_is_refused() {
        let mut state = LocalState::default();
        state.bind("4", binding(Host::Local, "ws4")).unwrap();
        state.migrate("4", &remote()).unwrap();
        let before = state.clone();

        let refused = state.migrate("4", &third());

        assert!(matches!(
            refused,
            Err(BindError::CleanupPending { ref left, .. }) if *left == Host::Local
        ));
        assert_eq!(state, before);
        state.migrate("4", &Host::Local).unwrap();
        assert_eq!(state.get("4").unwrap().host(), &Host::Local);
        assert_eq!(cleanup_of(&state, "4"), Some(remote()));
    }

    #[test]
    fn migrate_refuses_an_unbound_workspace_or_a_session_bound_there() {
        let mut state = LocalState::default();
        assert!(matches!(
            state.migrate("4", &remote()),
            Err(BindError::NotBound(ws)) if ws == "4"
        ));
        state.bind("4", binding(Host::Local, "ws4")).unwrap();
        state.bind("5", binding(remote(), "ws4")).unwrap();
        let before = state.clone();
        assert!(matches!(
            state.migrate("4", &remote()),
            Err(BindError::SessionBoundElsewhere { workspace, .. }) if workspace == "5"
        ));
        assert_eq!(state, before);
    }

    /// Guarantee 5: a binding with a pending cleanup cannot be removed
    /// until the cleanup ends, and binding it again as it is keeps the
    /// cleanup.
    #[test]
    fn a_pending_cleanup_is_never_lost() {
        let mut state = LocalState::default();
        state.bind("4", binding(Host::Local, "ws4")).unwrap();
        state.migrate("4", &remote()).unwrap();
        let ws4 = SessionName::new("ws4").unwrap();

        assert!(matches!(
            state.unbind("4"),
            Err(BindError::CleanupPending { .. })
        ));
        assert!(matches!(
            state.unbind_session(&remote(), &ws4),
            Err(BindError::CleanupPending { .. })
        ));
        state.bind("4", binding(remote(), "ws4")).unwrap();
        assert_eq!(cleanup_of(&state, "4"), Some(Host::Local));
        state.finish_cleanup("4");
        assert_eq!(
            state.unbind_session(&remote(), &ws4).unwrap().as_deref(),
            Some("4")
        );
    }

    #[test]
    fn unbind_and_unbind_session() {
        let mut state = sample_state("3");
        state.bind("q", binding(Host::Local, "notes")).unwrap();
        assert_eq!(
            state.unbind("3").unwrap(),
            Some(binding(remote(), "mysession"))
        );
        assert_eq!(state.unbind("3").unwrap(), None);
        let notes = SessionName::new("notes").unwrap();
        assert_eq!(
            state
                .unbind_session(&Host::Local, &notes)
                .unwrap()
                .as_deref(),
            Some("q")
        );
        assert_eq!(state.unbind_session(&Host::Local, &notes).unwrap(), None);
        assert_eq!(state, LocalState::default());
    }

    // ---------- persistence ----------

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let original = sample_state("1");
        store.save(&original).unwrap();
        assert_eq!(store.load().unwrap(), original);
    }

    #[test]
    fn state_file_format() {
        let json = serde_json::to_value(sample_state("4")).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"workspaces": {"4": {"host": "user@host", "session": "mysession"}}})
        );
        let mut moved = sample_state("4");
        moved.migrate("4", &Host::Local).unwrap();
        assert_eq!(
            serde_json::to_value(&moved).unwrap(),
            serde_json::json!({"workspaces": {"4": {"host": "local", "session": "mysession",
                "cleanup": {"host": "user@host"}}}})
        );
    }

    /// A pending cleanup round-trips through `state.json`. One that names
    /// its binding's own host would end the session's live shells: such a
    /// file is quarantined, as any invalid state is.
    #[test]
    fn a_pending_cleanup_is_saved_and_one_of_its_own_host_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut moved = sample_state("4");
        moved.migrate("4", &Host::Local).unwrap();
        store.save(&moved).unwrap();
        assert_eq!(store.load().unwrap(), moved);

        fs::write(
            &store.path,
            r#"{"workspaces": {"4": {"host": "local", "session": "a", "cleanup": {"host": "local"}}}}"#,
        )
        .unwrap();
        assert_eq!(store.lock().unwrap().state(), &LocalState::default());
        assert!(store.path.with_extension("json.corrupt").exists());
    }

    /// PLAN-terminal-here decision 2: a binding keeps its window command
    /// across a save, and a binding without one writes no field, so a file
    /// of an earlier i3mux reads the same.
    #[test]
    fn a_window_command_is_saved_with_its_binding() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let command = crate::types::WindowTemplate::new("w {shell}").unwrap();
        let mut state = LocalState::default();
        state
            .bind(
                "4",
                binding(remote(), "ws4").with_window_command(Some(command.clone())),
            )
            .unwrap();
        state.bind("5", binding(Host::Local, "ws5")).unwrap();
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.get("4").unwrap().window_command(), Some(&command));
        assert_eq!(loaded.get("5").unwrap().window_command(), None);
        let text = fs::read_to_string(dir.path().join("state.json")).unwrap();
        assert_eq!(text.matches("window_command").count(), 1, "{text}");
    }

    #[test]
    fn load_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(store_in(dir.path()).load().unwrap(), LocalState::default());
    }

    #[test]
    fn corrupt_state_is_quarantined_under_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        fs::write(&store.path, "{ this is not valid json }").unwrap();

        let lock = store.lock().unwrap();
        assert_eq!(lock.state(), &LocalState::default());
        drop(lock);

        assert!(!store.path.exists());
        let quarantine = store.path.with_extension("json.corrupt");
        assert_eq!(
            fs::read_to_string(&quarantine).unwrap(),
            "{ this is not valid json }"
        );
    }

    #[test]
    fn unlocked_load_never_moves_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        fs::write(&store.path, "{ this is not valid json }").unwrap();

        assert_eq!(store.load().unwrap(), LocalState::default());

        assert!(store.path.exists());
        assert!(!store.path.with_extension("json.corrupt").exists());
    }

    #[test]
    fn pre_shpool_state_is_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let old = r#"{"workspaces": {"4": {"session_type": "local", "host": "local",
            "session_name": null, "next_socket_id": 2, "sockets": {}}}}"#;
        fs::write(&store.path, old).unwrap();
        assert_eq!(store.lock().unwrap().state(), &LocalState::default());
        assert!(store.path.with_extension("json.corrupt").exists());
    }

    #[test]
    fn invalid_names_in_state_are_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        fs::write(
            &store.path,
            r#"{"workspaces": {"4": {"host": "-x", "session": "a"}}}"#,
        )
        .unwrap();
        assert_eq!(store.lock().unwrap().state(), &LocalState::default());
        assert!(store.path.with_extension("json.corrupt").exists());
    }

    #[test]
    fn failed_save_leaves_previous_file_intact() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let original = sample_state("5");
        store.save(&original).unwrap();

        set_readonly(dir.path(), true);
        let err = store.save(&LocalState::default());
        set_readonly(dir.path(), false);
        assert!(err.is_err(), "save should fail on a read-only directory");

        assert_eq!(store.load().unwrap(), original);
    }

    #[test]
    fn save_produces_valid_json_and_no_temp_leftover() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.save(&sample_state("1")).unwrap();
        let raw = fs::read_to_string(&store.path).unwrap();
        serde_json::from_str::<LocalState>(&raw).unwrap();
        assert!(!store.path.with_extension("json.tmp").exists());
    }

    #[test]
    fn uncommitted_changes_are_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        {
            let mut lock = store.lock().unwrap();
            lock.state_mut()
                .bind("1", binding(Host::Local, "a"))
                .unwrap();
        }
        assert_eq!(store.load().unwrap(), LocalState::default());
        {
            let mut lock = store.lock().unwrap();
            lock.state_mut()
                .bind("1", binding(Host::Local, "a"))
                .unwrap();
            lock.commit().unwrap();
            lock.state_mut()
                .bind("2", binding(Host::Local, "b"))
                .unwrap();
        }
        assert_eq!(
            store.load().unwrap().get("1"),
            Some(&binding(Host::Local, "a"))
        );
        assert_eq!(store.load().unwrap().get("2"), None);
    }

    #[test]
    fn with_lock_writes_nothing_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let result: Result<()> = store.with_lock(|s| {
            s.bind("1", binding(Host::Local, "a"))?;
            anyhow::bail!("stop")
        });
        assert!(result.is_err());
        assert!(!store.path.exists());
    }

    // ---------- concurrency (the flock seam) ----------

    #[test]
    fn concurrent_binds_are_never_lost() {
        const THREADS: usize = 16;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(store_in(dir.path()));
        let barrier = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .with_lock(|s| {
                            Ok(s.bind(&i.to_string(), binding(Host::Local, &format!("s{i}")))?)
                        })
                        .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        // Without the lock, racing load -> bind -> save loses bindings.
        assert_eq!(store.load().unwrap().bindings().count(), THREADS);
    }

    #[test]
    fn concurrent_binds_of_one_session_bind_exactly_once() {
        const THREADS: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(store_in(dir.path()));
        let barrier = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.with_lock(|s| Ok(s.bind(&i.to_string(), binding(Host::Local, "shared"))?))
                })
            })
            .collect();
        let ok = handles.into_iter().flat_map(|h| h.join().unwrap()).count();
        assert_eq!(ok, 1);
        assert_eq!(store.load().unwrap().bindings().count(), 1);
    }

    fn set_readonly(path: &Path, readonly: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mode = if readonly { 0o500 } else { 0o700 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
}
