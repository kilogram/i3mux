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
//! 5. **Old files load unchanged** (plan-layout-backup rule 19): fields added
//!    to a binding have serde defaults, and a binding without them saves
//!    back to the same JSON.
//! 6. **Every new binding has a token and an owner** (plan-layout-backup
//!    decision 2): [`WorkspaceState::new`] needs both. Only a binding loaded
//!    from a file written before tokens or owners lacks them.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::types::{Host, SessionName, SwayInstance, Token};

/// What a workspace is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceState {
    host: Host,
    session: SessionName,
    /// The token of the take that made this binding. `None` only in a
    /// binding from before tokens; it holds the session while the host has
    /// no token file (plan-layout-backup rule 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<Token>,
    /// The Sway instance that holds this binding's windows. A binding that
    /// another instance owns, or none, is resumable (plan-layout-backup
    /// decision 4). `None` in a binding from before owners.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<SwayInstance>,
}

impl WorkspaceState {
    /// A binding to `session` on `host`, made by the take that returned
    /// `token`, with its windows in the Sway `owner`.
    #[must_use]
    pub fn new(host: Host, session: SessionName, token: Token, owner: SwayInstance) -> Self {
        Self {
            host,
            session,
            token: Some(token),
            owner: Some(owner),
        }
    }

    /// A binding as a file from before tokens and owners has it.
    #[cfg(test)]
    #[must_use]
    pub fn without_token(host: Host, session: SessionName) -> Self {
        Self {
            host,
            session,
            token: None,
            owner: None,
        }
    }

    /// The same binding, with its windows in the Sway `owner`.
    #[must_use]
    pub fn with_owner(self, owner: SwayInstance) -> Self {
        Self {
            owner: Some(owner),
            ..self
        }
    }

    #[must_use]
    pub fn host(&self) -> &Host {
        &self.host
    }

    #[must_use]
    pub fn session(&self) -> &SessionName {
        &self.session
    }

    /// The token this binding took, if it has one.
    #[must_use]
    pub fn token(&self) -> Option<&Token> {
        self.token.as_ref()
    }

    /// The Sway that holds this binding's windows, if it has an owner.
    #[must_use]
    pub fn owner(&self) -> Option<&SwayInstance> {
        self.owner.as_ref()
    }

    /// Whether the Sway `sway` owns this binding.
    #[must_use]
    pub fn is_owned_by(&self, sway: &SwayInstance) -> bool {
        self.owner.as_ref() == Some(sway)
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
}

/// All bindings, keyed by workspace name, as persisted to `state.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalState {
    workspaces: BTreeMap<String, WorkspaceState>,
}

impl LocalState {
    /// Whether `ws` may bind to (`host`, `session`) under guarantee 1. The
    /// same check as [`LocalState::bind`], so that a caller can refuse before
    /// it takes the session on its host (plan-layout-backup rule 2).
    ///
    /// # Errors
    /// As [`LocalState::bind`].
    pub fn check_bind(
        &self,
        ws: &str,
        host: &Host,
        session: &SessionName,
    ) -> Result<(), BindError> {
        if let Some(other) = self.workspace_of(host, session) {
            if other != ws {
                return Err(BindError::SessionBoundElsewhere {
                    host: host.clone(),
                    session: session.clone(),
                    workspace: other.to_owned(),
                });
            }
        }
        match self.workspaces.get(ws) {
            Some(current) if &current.host != host || &current.session != session => {
                Err(BindError::WorkspaceTaken {
                    workspace: ws.to_owned(),
                    host: current.host.clone(),
                    session: current.session.clone(),
                })
            }
            Some(_) | None => Ok(()),
        }
    }

    /// Bind workspace `ws` to `binding`. If `ws` is already bound to the
    /// same host and session, `binding` replaces that binding, so the token
    /// of the latest take wins.
    ///
    /// # Errors
    /// [`BindError::SessionBoundElsewhere`] if another workspace has this
    /// (host, session); [`BindError::WorkspaceTaken`] if `ws` is bound to a
    /// different session.
    pub fn bind(&mut self, ws: &str, binding: WorkspaceState) -> Result<(), BindError> {
        self.check_bind(ws, &binding.host, &binding.session)?;
        self.workspaces.insert(ws.to_owned(), binding);
        Ok(())
    }

    /// Remove the binding of `ws`, returning it.
    pub fn unbind(&mut self, ws: &str) -> Option<WorkspaceState> {
        self.workspaces.remove(ws)
    }

    /// Remove the binding of (`host`, `session`), returning its workspace.
    pub fn unbind_session(&mut self, host: &Host, session: &SessionName) -> Option<String> {
        let ws = self.workspace_of(host, session)?.to_owned();
        self.workspaces.remove(&ws);
        Some(ws)
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

    /// Where `state.json` is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
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

    /// Replace the file atomically with [`replace_file`]. A failed save
    /// leaves the old file intact.
    ///
    /// # Errors
    /// If the directory or temp file cannot be written.
    pub fn save(&self, state: &LocalState) -> Result<()> {
        let contents = serde_json::to_string_pretty(state).context("cannot encode the state")?;
        replace_file(&self.path, &contents)
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

/// Replace the JSON file at `path` atomically: create its directory, write
/// a sibling `.tmp` file, fsync it, rename it over `path`. Readers see the
/// old or the new file, never a part. The temp name is fixed, so callers
/// hold the state lock.
///
/// # Errors
/// If the directory or temp file cannot be written; `path` is then
/// unchanged.
pub fn replace_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
        file.write_all(contents.as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("cannot write {}", tmp.display()))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))
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
        WorkspaceState::without_token(host, SessionName::new(session).unwrap())
    }

    fn token(digit: char) -> Token {
        Token::parse(&digit.to_string().repeat(32)).unwrap()
    }

    fn sway(pid: u32) -> SwayInstance {
        SwayInstance::new("boot-1", format!("/run/user/1000/sway-ipc.1000.{pid}.sock"))
    }

    fn taken(host: Host, session: &str, digit: char) -> WorkspaceState {
        let session = SessionName::new(session).unwrap();
        WorkspaceState::new(host, session, token(digit), sway(1))
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

    #[test]
    fn rebinding_the_same_session_replaces_its_token() {
        // Plan-layout-backup decision 2 and rule 21: the binding records
        // the token of its latest take.
        let mut state = LocalState::default();
        state.bind("4", binding(remote(), "ws4")).unwrap();
        state.bind("4", taken(remote(), "ws4", 'a')).unwrap();
        state.bind("4", taken(remote(), "ws4", 'b')).unwrap();
        assert_eq!(state.get("4").unwrap().token(), Some(&token('b')));
        assert_eq!(state.bindings().count(), 1);
    }

    #[test]
    fn check_bind_refuses_what_bind_refuses() {
        let mut state = LocalState::default();
        state.bind("4", taken(remote(), "ws4", 'a')).unwrap();
        let ws4 = SessionName::new("ws4").unwrap();
        let other = SessionName::new("other").unwrap();
        for (ws, session) in [("5", &ws4), ("4", &other)] {
            let checked = state.check_bind(ws, &remote(), session).unwrap_err();
            let bound = state
                .clone()
                .bind(
                    ws,
                    WorkspaceState::new(remote(), session.clone(), token('b'), sway(1)),
                )
                .unwrap_err();
            assert_eq!(checked, bound);
        }
        state.check_bind("4", &remote(), &ws4).unwrap();
        state.check_bind("5", &remote(), &other).unwrap();
    }

    #[test]
    fn unbind_and_unbind_session() {
        let mut state = sample_state("3");
        state.bind("q", binding(Host::Local, "notes")).unwrap();
        assert_eq!(state.unbind("3"), Some(binding(remote(), "mysession")));
        assert_eq!(state.unbind("3"), None);
        let notes = SessionName::new("notes").unwrap();
        assert_eq!(
            state.unbind_session(&Host::Local, &notes).as_deref(),
            Some("q")
        );
        assert_eq!(state.unbind_session(&Host::Local, &notes), None);
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
    }

    #[test]
    fn binding_with_a_token_and_an_owner_saves_both() {
        let mut state = LocalState::default();
        state.bind("4", taken(remote(), "ws4", 'c')).unwrap();
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"workspaces": {"4": {
                "host": "user@host", "session": "ws4", "token": "c".repeat(32),
                "owner": {"boot_id": "boot-1",
                          "socket": "/run/user/1000/sway-ipc.1000.1.sock"}}}})
        );
        assert_eq!(serde_json::from_value::<LocalState>(json).unwrap(), state);
    }

    #[test]
    fn a_binding_is_owned_only_by_its_own_sway() {
        // Plan-layout-backup decision 2: another boot or another Sway run is
        // another owner.
        let owned = taken(remote(), "ws4", 'a');
        assert!(owned.is_owned_by(&sway(1)));
        assert!(!owned.is_owned_by(&sway(2)));
        let rebooted = SwayInstance::new("boot-2", "/run/user/1000/sway-ipc.1000.1.sock");
        assert!(!owned.is_owned_by(&rebooted));
        let legacy = binding(remote(), "ws4");
        assert!(!legacy.is_owned_by(&sway(1)));
        let adopted = legacy.with_owner(sway(2));
        assert!(adopted.is_owned_by(&sway(2)));
        assert_eq!(adopted.token(), None, "an owner changes nothing else");
    }

    #[test]
    fn state_from_before_tokens_loads_and_saves_unchanged() {
        // Plan-layout-backup rule 19. The fixture is the format of i3mux
        // before tokens.
        let fixture = include_str!("testdata/state-before-tokens.json");
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        fs::write(&store.path, fixture).unwrap();

        let lock = store.lock().unwrap();
        let state = lock.state().clone();
        drop(lock);

        assert!(!store.path.with_extension("json.corrupt").exists());
        let bound: Vec<(&str, &str, &str)> = state
            .bindings()
            .map(|(ws, b)| (ws, b.host().label(), b.session().as_str()))
            .collect();
        assert_eq!(
            bound,
            [
                ("3", "deepthought", "ws3"),
                ("4", "local", "ws4"),
                ("mail", "user@mail.example", "mail"),
            ]
        );
        assert!(state
            .bindings()
            .all(|(_, b)| b.token().is_none() && b.owner().is_none()));

        store.save(&state).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&store.path).unwrap()).unwrap();
        let original: serde_json::Value = serde_json::from_str(fixture).unwrap();
        assert_eq!(saved, original);
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
