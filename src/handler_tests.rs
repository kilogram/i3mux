//! Tests of the core command functions against fakes: an in-memory host
//! (shells and files), the in-memory Sway, and a launcher that opens fake
//! windows. Each test names the plan rule it backs.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::host::CheckedWrite;
use crate::host::HostError;
use crate::layout::Layout;
use crate::master::{MasterError, TrackedMasters, UnitName};
use crate::sway_ops::testing::FakeSway;
use crate::types::{RemoteHost, Token};

// ------------------------------------------------------------------ fakes

#[derive(Default)]
struct HostState {
    /// Live shells: name -> attached.
    shells: BTreeMap<String, bool>,
    files: BTreeMap<String, String>,
    fail_writes: bool,
    unreachable: bool,
    /// Like shpool, a detached shell still lists as attached for this many
    /// `list` calls.
    detach_lag: usize,
    /// Shells being detached: name -> lists left before they show detached.
    detaching: BTreeMap<String, usize>,
    /// Ignore `shpool detach` (clients that never let go).
    stuck_clients: bool,
    /// Every `shpool detach` call, by its names.
    detach_calls: Vec<Vec<String>>,
    /// Takes so far; the next token is made from this count.
    takes: u64,
    /// Token files: session -> content.
    tokens: BTreeMap<String, String>,
    /// Make `remove_file` fail with this stderr.
    remove_error: Option<&'static str>,
}

impl HostState {
    /// One `list` call passes: lagging detaches take effect.
    fn advance_detaches(&mut self) {
        let done: Vec<String> = self
            .detaching
            .iter()
            .filter(|(_, left)| **left == 0)
            .map(|(name, _)| name.clone())
            .collect();
        for name in done {
            self.detaching.remove(&name);
            if let Some(attached) = self.shells.get_mut(&name) {
                *attached = false;
            }
        }
        for left in self.detaching.values_mut() {
            *left -= 1;
        }
    }
}

/// An in-memory host. Understands `shpool list --json` and `shpool kill`
/// by the argv tail; everything else succeeds silently. Like `HostRunner`,
/// every operation on a remote host first starts the host's ssh master.
/// Token files live in their own map, as they live in the runtime directory
/// on a real host. Each take writes a new token, and the mutex makes token
/// operations exclude each other, as `flock` does on a real host.
#[derive(Clone)]
struct FakeHost {
    host: Host,
    state: Arc<Mutex<HostState>>,
    masters: Arc<dyn Masters + Send + Sync>,
}

impl FakeHost {
    fn new(host: Host, masters: Arc<dyn Masters + Send + Sync>) -> Self {
        Self {
            host,
            state: Arc::default(),
            masters,
        }
    }

    fn add_shell(&self, name: &str, attached: bool) {
        self.state
            .lock()
            .unwrap()
            .shells
            .insert(name.to_owned(), attached);
    }

    fn shells(&self) -> BTreeMap<String, bool> {
        self.state.lock().unwrap().shells.clone()
    }

    fn file(&self, path: &str) -> Option<String> {
        self.state.lock().unwrap().files.get(path).cloned()
    }

    fn failure(&self, stderr: &str) -> HostError {
        HostError::Failed {
            host: self.host.clone(),
            code: Some(1),
            stderr: stderr.to_owned(),
            command: String::new(),
        }
    }

    /// What every `HostRunner` operation does first: start the master of a
    /// remote host, then reach the host.
    fn connect(&self) -> Result<(), HostError> {
        if let Host::Ssh(remote) = &self.host {
            self.masters.ensure_started(remote)?;
        }
        if self.state.lock().unwrap().unreachable {
            return Err(HostError::Unreachable {
                host: RemoteHost::new("fake").unwrap(),
                detail: "Connection refused".into(),
            });
        }
        Ok(())
    }
}

impl HostExec for FakeHost {
    fn host(&self) -> &Host {
        &self.host
    }

    fn run(&self, argv: &[String], _stdin: Option<&str>) -> Result<String, HostError> {
        self.connect()?;
        let mut state = self.state.lock().unwrap();
        if argv.ends_with(&["list".to_owned(), "--json".to_owned()]) {
            state.advance_detaches();
            let sessions: Vec<_> = state
                .shells
                .iter()
                .map(|(name, attached)| {
                    let status = if *attached {
                        "Attached"
                    } else {
                        "Disconnected"
                    };
                    serde_json::json!({"name": name, "status": status})
                })
                .collect();
            return Ok(serde_json::json!({ "sessions": sessions }).to_string());
        }
        if let Some(at) = argv.iter().position(|a| a == "kill") {
            for name in &argv[at + 1..] {
                state.shells.remove(name);
            }
        }
        if let Some(at) = argv.iter().position(|a| a == "detach") {
            let names = argv[at + 1..].to_vec();
            if !state.stuck_clients {
                let lag = state.detach_lag;
                for name in &names {
                    state.detaching.insert(name.clone(), lag);
                }
            }
            state.detach_calls.push(names);
        }
        Ok(String::new())
    }

    fn attach_argv(&self, argv: &[String]) -> Vec<String> {
        argv.to_vec()
    }

    fn read_file(&self, path: &str) -> Result<Option<String>, HostError> {
        self.connect()?;
        Ok(self.file(path))
    }

    fn list_dir(&self, dir: &str) -> Result<Vec<String>, HostError> {
        self.connect()?;
        let prefix = format!("{dir}/");
        let state = self.state.lock().unwrap();
        Ok(state
            .files
            .keys()
            .filter_map(|p| p.strip_prefix(&prefix).map(str::to_owned))
            .collect())
    }

    fn remove_file(&self, path: &str) -> Result<bool, HostError> {
        self.connect()?;
        let mut state = self.state.lock().unwrap();
        if let Some(stderr) = state.remove_error {
            return Err(self.failure(stderr));
        }
        Ok(state.files.remove(path).is_some())
    }

    fn take_token(&self, session: &SessionName) -> Result<String, HostError> {
        self.connect()?;
        let mut state = self.state.lock().unwrap();
        if state.fail_writes {
            return Err(self.failure("No space left on device"));
        }
        state.takes += 1;
        let token = format!("{:032x}", state.takes);
        state
            .tokens
            .insert(session.to_string(), format!("{token}\n"));
        Ok(token)
    }

    fn read_token(&self, session: &SessionName) -> Result<Option<String>, HostError> {
        self.connect()?;
        Ok(self
            .state
            .lock()
            .unwrap()
            .tokens
            .get(session.as_str())
            .cloned())
    }

    fn remove_token(&self, session: &SessionName) -> Result<(), HostError> {
        self.connect()?;
        self.state.lock().unwrap().tokens.remove(session.as_str());
        Ok(())
    }

    fn write_file_if_token(
        &self,
        path: &str,
        content: &str,
        session: &SessionName,
        token: Option<&Token>,
    ) -> Result<CheckedWrite, HostError> {
        self.connect()?;
        let mut state = self.state.lock().unwrap();
        let found = state.tokens.get(session.as_str()).map(|t| t.trim_end());
        match (found, token) {
            (Some(found), _) if Some(found) != token.map(Token::as_str) => {
                return Ok(CheckedWrite::OtherToken)
            }
            (None, Some(_)) => return Ok(CheckedWrite::NoTokenFile),
            _ => {}
        }
        if state.fail_writes {
            return Err(self.failure("No space left on device"));
        }
        state.files.insert(path.to_owned(), content.to_owned());
        Ok(CheckedWrite::Written)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MasterEvent {
    Ensure(RemoteHost),
    Stop(RemoteHost),
}

#[derive(Default)]
struct MasterLog {
    events: Vec<MasterEvent>,
    /// Make `ensure_started` fail.
    fail_start: bool,
    /// Report no masters, as on a machine without systemd.
    direct: bool,
}

/// Records, in order, each start and stop of a host's master. Every master
/// is `i3mux-ssh-fake.service`, active with 2 restarts.
#[derive(Clone, Default)]
struct FakeMasters {
    log: Arc<Mutex<MasterLog>>,
}

impl FakeMasters {
    fn unit() -> UnitName {
        UnitName::from_escaped_host("fake").unwrap()
    }

    fn events(&self) -> Vec<MasterEvent> {
        self.log.lock().unwrap().events.clone()
    }

    fn ensured(&self) -> Vec<RemoteHost> {
        let events = self.events().into_iter();
        events
            .filter_map(|e| match e {
                MasterEvent::Ensure(h) => Some(h),
                MasterEvent::Stop(_) => None,
            })
            .collect()
    }

    fn stopped(&self) -> Vec<RemoteHost> {
        let events = self.events().into_iter();
        events
            .filter_map(|e| match e {
                MasterEvent::Stop(h) => Some(h),
                MasterEvent::Ensure(_) => None,
            })
            .collect()
    }

    /// Whether the master of `host` runs: its last event is a start. A
    /// remote command after a stop starts it again, so `false` after a stop
    /// also means that nothing ran on the host after it.
    fn running(&self, host: &RemoteHost) -> bool {
        self.events()
            .iter()
            .rev()
            .find(|e| matches!(e, MasterEvent::Ensure(h) | MasterEvent::Stop(h) if h == host))
            .is_some_and(|e| matches!(e, MasterEvent::Ensure(_)))
    }
}

impl Masters for FakeMasters {
    fn ensure_started(&self, host: &RemoteHost) -> Result<Link, MasterError> {
        let mut log = self.log.lock().unwrap();
        log.events.push(MasterEvent::Ensure(host.clone()));
        if log.fail_start {
            return Err(MasterError::StartFailed {
                host: host.clone(),
                unit: Self::unit(),
                detail: "ssh: Could not resolve hostname".into(),
            });
        }
        Ok(if log.direct {
            Link::Direct
        } else {
            Link::Shared(Self::unit())
        })
    }

    fn stop(&self, host: &RemoteHost) -> Result<(), MasterError> {
        self.log
            .lock()
            .unwrap()
            .events
            .push(MasterEvent::Stop(host.clone()));
        Ok(())
    }

    fn state(&self, _host: &RemoteHost) -> Result<MasterState, MasterError> {
        Ok(MasterState::Unit {
            unit: Self::unit(),
            active_state: "active".into(),
            restarts: 2,
        })
    }
}

/// Opens a window on the fake Sway (on `workspace`) and, like `shpool
/// attach`, creates or attaches the shell on the fake host. Can be told to
/// fail after some launches.
struct FakeLauncher {
    sway: Arc<FakeSway>,
    host: FakeHost,
    workspace: String,
    launches: Mutex<Vec<WindowCommand>>,
    fail_after: Option<usize>,
}

impl FakeLauncher {
    /// The argv of each launch, in order.
    fn argvs(&self) -> Vec<Vec<String>> {
        let launches = self.launches.lock().unwrap();
        launches.iter().map(|c| c.argv().to_vec()).collect()
    }
}

impl Launcher for FakeLauncher {
    fn spawn(&self, app_id: &str, _title: &str, command: &WindowCommand) -> Result<()> {
        let mut launches = self.launches.lock().unwrap();
        if self.fail_after.is_some_and(|n| launches.len() >= n) {
            anyhow::bail!("terminal crashed");
        }
        launches.push(command.clone());
        let mark = Mark::parse(app_id).unwrap();
        self.host.add_shell(&mark.terminal().to_string(), true);
        self.sway.open(&self.workspace, app_id);
        Ok(())
    }
}

/// One test world: a state store in a temp dir, a fake Sway, a fake host
/// (the only host), and a launcher that opens windows on workspace "4".
struct World {
    dir: tempfile::TempDir,
    store: StateStore,
    sway: Arc<FakeSway>,
    host: FakeHost,
    launcher: FakeLauncher,
    hosts: Box<HostFactory>,
    /// The masters' log; `masters` wraps it as `run` does.
    fakes: FakeMasters,
    masters: Arc<TrackedMasters>,
    backups: BackupStore,
    /// The Sway the commands run in.
    current_sway: SwayInstance,
}

impl World {
    fn new(host: Host) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        let sway = Arc::new(FakeSway::default());
        let fakes = FakeMasters::default();
        let masters = Arc::new(TrackedMasters::new(Box::new(fakes.clone())));
        let host = FakeHost::new(host, masters.clone());
        let launcher = FakeLauncher {
            sway: Arc::clone(&sway),
            host: host.clone(),
            workspace: "4".into(),
            launches: Mutex::default(),
            fail_after: None,
        };
        let fake = host.clone();
        let hosts: Box<HostFactory> = Box::new(move |h: &Host| {
            assert_eq!(h, fake.host(), "the world has one host");
            Box::new(fake.clone())
        });
        let backups = BackupStore::new(dir.path().join("backups"));
        Self {
            dir,
            store,
            sway,
            host,
            launcher,
            hosts,
            fakes,
            masters,
            backups,
            current_sway: SwayInstance::new("boot-2", "/run/user/1000/sway-ipc.1000.200.sock"),
        }
    }

    /// Run a core function as `run` runs a command: then stop the masters
    /// that no workspace uses.
    fn finish<T>(&self, command: impl FnOnce(&Ports) -> Result<T>) -> Result<T> {
        with_unused_masters_stopped(&self.store, &self.masters, &self.current_sway, || {
            command(&self.ports())
        })
    }

    fn ports(&self) -> Ports<'_> {
        Ports {
            store: &self.store,
            sway: self.sway.as_ref(),
            launcher: &self.launcher,
            hosts: self.hosts.as_ref(),
            masters: self.masters.as_ref(),
            log: Log { verbose: false },
            disconnect_wait: Duration::from_millis(300),
            backups: &self.backups,
            current_sway: &self.current_sway,
        }
    }

    /// Bind `workspace` as a `state.json` from before tokens has it: no
    /// token, and no take on the host.
    fn bind(&self, workspace: &str, session: &str) {
        let binding = WorkspaceState::without_token(self.host.host().clone(), name(session));
        self.store
            .with_lock(|s| Ok(s.bind(workspace, binding)?))
            .unwrap();
    }

    /// The token of `workspace`'s binding.
    fn bound_token(&self, workspace: &str) -> Option<Token> {
        let state = self.store.load().unwrap();
        state.get(workspace).and_then(|b| b.token().cloned())
    }

    /// The token file of `session` on the host.
    fn host_token(&self, session: &str) -> Option<String> {
        let state = self.host.state.lock().unwrap();
        state.tokens.get(session).map(|t| t.trim_end().to_owned())
    }

    /// The binding of `workspace`.
    fn binding(&self, workspace: &str) -> Option<WorkspaceState> {
        self.store.load().unwrap().get(workspace).cloned()
    }

    /// A binding left by a Sway that died: this machine took `session`, bound
    /// `workspace` in an earlier Sway, and backed up `backup` (if any) while
    /// it ran. No window of it is open now.
    fn crashed(&self, workspace: &str, session: &str, backup: Option<&[u32]>) -> WorkspaceState {
        let token = session::take(&self.host, &name(session)).unwrap();
        let binding = WorkspaceState::new(
            self.host.host().clone(),
            name(session),
            token,
            earlier_sway(),
        );
        self.store
            .with_lock(|s| Ok(s.bind(workspace, binding.clone())?))
            .unwrap();
        if let Some(numbers) = backup {
            self.backups
                .write(&binding, &hsplit(session, numbers))
                .unwrap();
        }
        binding
    }

    /// The backed-up layout of `binding`.
    fn backup(&self, binding: &WorkspaceState) -> Option<Layout> {
        self.backups.read(binding).unwrap()
    }

    /// Whether a backup file of `session` exists, whatever its token.
    fn has_backup_file(&self, session: &str) -> bool {
        let label = self.host.host().label();
        let file = format!("backups/{label}/{session}.json");
        self.dir.path().join(file).exists()
    }

    /// How many takes the host has seen.
    fn takes(&self) -> u64 {
        self.host.state.lock().unwrap().takes
    }

    /// Another machine takes `session`, as its `attach` would.
    fn take_elsewhere(&self, session: &str) -> Token {
        session::take(&self.host, &name(session)).unwrap()
    }

    fn mark(&self, terminal: &str) -> Mark {
        Mark::new(self.host.host().clone(), terminal.parse().unwrap())
    }

    /// Terminals with windows, as numbers.
    fn window_numbers(&self, session: &str) -> Vec<u32> {
        let tree = self.sway.get_tree().unwrap();
        let mut numbers: Vec<u32> =
            window::session_terminals(&tree, self.host.host(), &name(session))
                .iter()
                .map(TerminalName::number)
                .collect();
        numbers.sort_unstable();
        numbers
    }

    fn save_layout(&self, session: &str, numbers: &[u32]) {
        let layout = hsplit(session, numbers);
        let saved = SavedSession::new(name(session), self.host.host().clone(), layout, 0).unwrap();
        // Written straight to the host, whatever its token: a layout that
        // some machine saved earlier.
        let path = format!(".local/state/i3mux/sessions/{session}.json");
        let json = serde_json::to_string_pretty(&saved).unwrap();
        self.host.state.lock().unwrap().files.insert(path, json);
    }
}

/// Terminals `numbers` of `session`, side by side.
fn hsplit(session: &str, numbers: &[u32]) -> Layout {
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

/// The Sway that ran before the current one of every [`World`].
fn earlier_sway() -> SwayInstance {
    SwayInstance::new("boot-1", "/run/user/1000/sway-ipc.1000.100.sock")
}

fn workspace(name: &str, num: i32) -> WorkspaceInfo {
    serde_json::from_value(serde_json::json!({"name": name, "num": num, "focused": true})).unwrap()
}

fn name(text: &str) -> SessionName {
    SessionName::new(text).unwrap()
}

fn t(session: &str, n: u32) -> TerminalName {
    TerminalName::new(&name(session), n)
}

fn command_error(err: &anyhow::Error) -> &CommandError {
    err.downcast_ref::<CommandError>()
        .unwrap_or_else(|| panic!("not a CommandError: {err:#}"))
}

const SAVED_WS4: &str = ".local/state/i3mux/sessions/ws4.json";

// ------------------------------------------------------------- terminal

#[test]
fn concurrent_terminals_never_share_a_name() {
    // Plan rule 4: under the state lock, list -> choose -> spawn -> mark.
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", false); // orphan
    world.host.add_shell("ws4-002", true); // attached from elsewhere
    world.host.add_shell("ws4-005", false); // orphan
    let ports = world.ports();

    let choices: Vec<Choice> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| open_terminal(&ports, "4", &TerminalRequest::default()).unwrap())
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut numbers: Vec<u32> = choices.iter().map(|c| c.name().number()).collect();
    numbers.sort_unstable();
    assert_eq!(numbers, [1, 5, 6, 7, 8, 9, 10, 11], "{choices:?}");
    let reattached = choices
        .iter()
        .filter(|c| matches!(c, Choice::Reattach(_)))
        .count();
    assert_eq!(reattached, 2);
    assert_eq!(world.window_numbers("ws4"), [1, 5, 6, 7, 8, 9, 10, 11]);
}

#[test]
fn terminal_takes_over_a_shell_attached_elsewhere_only_with_force() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", true);
    let ports = world.ports();

    let plain = open_terminal(&ports, "4", &TerminalRequest::default()).unwrap();
    assert_eq!(plain, Choice::Create(t("ws4", 2)));

    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", true);
    let forced = TerminalRequest {
        force: true,
        ..TerminalRequest::default()
    };
    assert_eq!(
        open_terminal(&world.ports(), "4", &forced).unwrap(),
        Choice::Reattach(t("ws4", 1))
    );
    let launch = &world.launcher.argvs()[0];
    assert_eq!(launch[launch.len() - 3..], ["attach", "-f", "ws4-001"]);
}

#[test]
fn terminal_with_a_command_always_starts_a_new_shell() {
    // Plan decision 14: an orphan runs a shell, not the command.
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", false);
    let request = TerminalRequest {
        command: Some("htop -d 5".into()),
        ..TerminalRequest::default()
    };
    assert_eq!(
        open_terminal(&world.ports(), "4", &request).unwrap(),
        Choice::Create(t("ws4", 2))
    );
    let launch = &world.launcher.argvs()[0];
    assert_eq!(
        launch[launch.len() - 4..],
        ["attach", "-c", "htop -d 5", "ws4-002"]
    );
}

#[test]
fn terminal_new_skips_orphans() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", false);
    let request = TerminalRequest {
        new: true,
        ..TerminalRequest::default()
    };
    assert_eq!(
        open_terminal(&world.ports(), "4", &request).unwrap(),
        Choice::Create(t("ws4", 2))
    );
}

#[test]
fn terminal_in_unbound_workspace_is_refused() {
    let world = World::new(Host::Local);
    let err = open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Workspace 4 is not bound to a session; run 'i3mux activate' first"
    );
}

// ------------------------------------------------------------- activate

#[test]
fn activate_binds_and_opens_the_first_terminal() {
    let world = World::new(Host::Local);
    let (session, choice) =
        activate(&world.ports(), &Host::Local, &workspace("4", 4), None).unwrap();
    assert_eq!(session, name("ws4"));
    assert_eq!(choice, Choice::Create(t("ws4", 1)));
    assert_eq!(
        world.store.load().unwrap().get("4").unwrap().session(),
        &name("ws4")
    );
    assert_eq!(world.window_numbers("ws4"), [1]);
}

#[test]
fn activate_takes_the_session_and_binds_its_token() {
    // Plan-layout-backup rule 1.
    let world = World::new(Host::Local);
    activate(&world.ports(), &Host::Local, &workspace("4", 4), None).unwrap();
    let first = world.bound_token("4").unwrap();
    assert_eq!(world.host_token("ws4").as_deref(), Some(first.as_str()));

    // Again in the same workspace: a new take, and the binding follows it.
    activate(&world.ports(), &Host::Local, &workspace("4", 4), None).unwrap();
    let second = world.bound_token("4").unwrap();
    assert_ne!(first, second);
    assert_eq!(world.host_token("ws4").as_deref(), Some(second.as_str()));
}

#[test]
fn refused_activate_never_takes() {
    // A refused activate must not take the session from the workspace that
    // holds it (plan-layout-backup rule 2).
    let world = World::new(Host::Local);
    activate(
        &world.ports(),
        &Host::Local,
        &workspace("5", 5),
        Some(name("ws4")),
    )
    .unwrap();
    let token = world.bound_token("5").unwrap();

    let err = activate(&world.ports(), &Host::Local, &workspace("4", 4), None).unwrap_err();

    assert!(err.to_string().contains("already bound"), "{err:#}");
    assert_eq!(world.takes(), 1);
    assert_eq!(world.host_token("ws4").as_deref(), Some(token.as_str()));
    assert_eq!(world.bound_token("5"), Some(token));
}

#[test]
fn activate_on_unreachable_host_binds_nothing() {
    // Plan rule 11: an ssh failure aborts; it is not an empty host.
    let remote = Host::Ssh(RemoteHost::new("deepthought").unwrap());
    let world = World::new(remote.clone());
    world.host.state.lock().unwrap().unreachable = true;
    let err = activate(&world.ports(), &remote, &workspace("4", 4), None).unwrap_err();
    assert!(err.to_string().contains("cannot reach"), "{err:#}");
    assert_eq!(world.store.load().unwrap().bindings().count(), 0);
}

#[test]
fn activate_named_workspace_uses_its_name_as_session() {
    let world = World::new(Host::Local);
    let (session, _) = activate(&world.ports(), &Host::Local, &workspace("q", -1), None).unwrap();
    assert_eq!(session, name("q"));
}

#[test]
fn activate_numbered_workspace_uses_ws_prefix() {
    let world = World::new(Host::Local);
    let (session, _) = activate(&world.ports(), &Host::Local, &workspace("4", 4), None).unwrap();
    assert_eq!(session, name("ws4"));
}

#[test]
fn activate_workspace_with_invalid_name_needs_a_session_name() {
    let world = World::new(Host::Local);
    let err = activate(&world.ports(), &Host::Local, &workspace("a:b", -1), None).unwrap_err();
    assert!(
        matches!(command_error(&err), CommandError::NoDefaultSession { workspace, .. } if workspace == "a:b")
    );
    assert!(err.to_string().contains("--session"), "{err:#}");
    let (session, _) = activate(
        &world.ports(),
        &Host::Local,
        &workspace("a:b", -1),
        Some(name("notes")),
    )
    .unwrap();
    assert_eq!(session, name("notes"));
}

// --------------------------------------------------------------- detach

#[test]
fn detach_requires_an_i3mux_window() {
    // Plan rule 5.
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.sway.open("4", "firefox");
    let err = detach(&world.ports(), "4", 0).unwrap_err();
    assert!(matches!(
        command_error(&err),
        CommandError::NothingToDetach { .. }
    ));
    assert!(
        world.store.load().unwrap().get("4").is_some(),
        "binding must stay"
    );
    assert_eq!(world.host.file(SAVED_WS4), None);
}

#[test]
fn failed_layout_write_leaves_every_window_open() {
    // Plan rule 7: the layout is written before any window closes.
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.sway.open_marked("4", &world.mark("ws4-001"));
    world.sway.open_marked("4", &world.mark("ws4-002"));
    world.host.state.lock().unwrap().fail_writes = true;

    let err = detach(&world.ports(), "4", 0).unwrap_err();

    assert!(
        err.to_string().contains("No space left on device"),
        "{err:#}"
    );
    assert!(
        world.sway.commands().iter().all(|c| !c.contains("kill")),
        "{:?}",
        world.sway.commands()
    );
    assert_eq!(world.window_numbers("ws4"), [1, 2]);
    assert!(
        world.store.load().unwrap().get("4").is_some(),
        "binding must stay"
    );
}

#[test]
fn detach_saves_then_closes_all_windows_and_unbinds() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.sway.open_marked("4", &world.mark("ws4-001"));
    world.sway.open_marked("4", &world.mark("ws4-002"));
    world.sway.open("4", "firefox");

    let detached = detach(&world.ports(), "4", 1_790_000_000).unwrap();

    assert_eq!(detached.terminals, 2);
    let saved = session::load(&world.host, &name("ws4")).unwrap().unwrap();
    assert_eq!(saved.layout().terminals(), [&t("ws4", 1), &t("ws4", 2)]);
    let kills: Vec<String> = world
        .sway
        .commands()
        .into_iter()
        .filter(|c| c.ends_with(" kill"))
        .collect();
    assert_eq!(kills.len(), 1, "one kill command closes every window");
    assert_eq!(world.window_numbers("ws4"), Vec::<u32>::new());
    assert_eq!(world.sway.window_count(), 1, "other windows stay");
    assert!(world.store.load().unwrap().get("4").is_none());
}

#[test]
fn detach_disconnects_the_closed_windows_shells_before_returning() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.state.lock().unwrap().detach_lag = 3;
    open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();
    open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();
    world.host.add_shell("ws4-003", true); // attached elsewhere, no window here

    detach(&world.ports(), "4", 0).unwrap();

    let state = world.host.state.lock().unwrap();
    assert_eq!(state.detach_calls, [["ws4-001", "ws4-002"]]);
    assert_eq!(state.shells.get("ws4-001"), Some(&false));
    assert_eq!(state.shells.get("ws4-002"), Some(&false));
    assert_eq!(
        state.shells.get("ws4-003"),
        Some(&true),
        "not ours to detach"
    );
}

#[test]
fn attach_right_after_detach_is_not_refused() {
    // The integration suite found this: Sway closes windows asynchronously,
    // so without the wait `attach` saw the shells as attached elsewhere.
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.state.lock().unwrap().detach_lag = 2;
    open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();
    open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    detach(&world.ports(), "4", 0).unwrap();
    let outcome = attach(&world.ports(), &Host::Local, "4", None, false).unwrap();

    assert!(matches!(outcome, AttachOutcome::Attached(_)), "{outcome:?}");
    assert_eq!(world.window_numbers("ws4"), [1, 2]);
}

#[test]
fn detach_reports_clients_that_never_let_go() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.state.lock().unwrap().stuck_clients = true;
    open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    let err = detach(&world.ports(), "4", 0).unwrap_err();

    assert!(
        matches!(command_error(&err), CommandError::StillAttached { terminals, .. } if terminals == "ws4-001"),
        "{err:#}"
    );
    assert!(err.to_string().contains("did not disconnect"), "{err:#}");
    // The detach itself happened: layout saved, windows closed, unbound.
    assert!(world.host.file(SAVED_WS4).is_some());
    assert_eq!(world.window_numbers("ws4"), Vec::<u32>::new());
    assert!(world.store.load().unwrap().get("4").is_none());
}

#[test]
fn detach_with_the_hosts_token_saves_the_layout() {
    // Plan-layout-backup rule 4.
    let world = World::new(Host::Local);
    activate(&world.ports(), &Host::Local, &workspace("4", 4), None).unwrap();
    open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();
    let token = world.bound_token("4").unwrap();
    assert_eq!(world.host_token("ws4").as_deref(), Some(token.as_str()));

    let detached = detach(&world.ports(), "4", 0).unwrap();

    assert_eq!(detached.token, TokenMatch::Same);
    let saved = session::load(&world.host, &name("ws4")).unwrap().unwrap();
    assert_eq!(saved.layout().terminals(), [&t("ws4", 1), &t("ws4", 2)]);
    assert_eq!(
        world.host.state.lock().unwrap().detach_calls,
        [["ws4-001", "ws4-002"]]
    );
    assert_eq!(
        world.host_token("ws4").as_deref(),
        Some(token.as_str()),
        "detach never takes"
    );
}

#[test]
fn detach_after_another_take_closes_and_unbinds_without_saving() {
    // Plan-layout-backup rule 5: the host layout and the shells' clients
    // belong to the machine that took the session.
    let world = World::new(Host::Local);
    activate(&world.ports(), &Host::Local, &workspace("4", 4), None).unwrap();
    open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();
    world.save_layout("ws4", &[7]);
    let theirs = world.take_elsewhere("ws4");
    let their_layout = world.host.file(SAVED_WS4);

    let detached = detach(&world.ports(), "4", 0).unwrap();

    assert_eq!(detached.token, TokenMatch::TakenElsewhere);
    assert_eq!(detached.terminals, 2);
    assert_eq!(world.host.file(SAVED_WS4), their_layout, "layout untouched");
    assert!(
        world.host.state.lock().unwrap().detach_calls.is_empty(),
        "no shpool detach"
    );
    assert_eq!(world.window_numbers("ws4"), Vec::<u32>::new());
    assert!(world.store.load().unwrap().get("4").is_none());
    assert_eq!(world.host_token("ws4").as_deref(), Some(theirs.as_str()));
    assert!(
        format_detached(&detached).contains("another machine took it since"),
        "{}",
        format_detached(&detached)
    );
}

#[test]
fn detach_of_a_binding_from_before_tokens_needs_a_host_without_one() {
    // Plan-layout-backup rule 4: no token matches no token file only.
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.sway.open_marked("4", &world.mark("ws4-001"));
    world.take_elsewhere("ws4");

    let detached = detach(&world.ports(), "4", 0).unwrap();

    assert_eq!(detached.token, TokenMatch::TakenElsewhere);
    assert_eq!(world.host.file(SAVED_WS4), None);
    assert!(world.store.load().unwrap().get("4").is_none());
}

#[test]
fn detach_on_an_unreachable_host_changes_nothing() {
    let remote = deepthought();
    let world = World::new(remote);
    world.bind("4", "ws4");
    world.sway.open_marked("4", &world.mark("ws4-001"));
    world.host.state.lock().unwrap().unreachable = true;

    let err = detach(&world.ports(), "4", 0).unwrap_err();

    assert!(err.to_string().contains("cannot reach"), "{err:#}");
    assert_eq!(world.window_numbers("ws4"), [1]);
    assert!(world.store.load().unwrap().get("4").is_some());
}

#[test]
fn detach_message_says_whether_the_layout_was_saved() {
    let detached = |token| Detached {
        session: name("ws4"),
        host: deepthought(),
        terminals: 2,
        token,
    };
    assert_eq!(
        format_detached(&detached(TokenMatch::Same)),
        "✓ Detached session 'ws4': layout of 2 terminals saved on deepthought"
    );
    assert_eq!(
        format_detached(&detached(TokenMatch::TakenElsewhere)),
        "✓ Closed session 'ws4' here: another machine took it since, \
         so the layout of these 2 terminals was not saved on deepthought"
    );
    assert_eq!(
        format_detached(&detached(TokenMatch::Ended)),
        "✓ Closed session 'ws4' here: it ended on deepthought, \
         so the layout of these 2 terminals was not saved"
    );
}

#[test]
fn detach_after_the_session_ended_closes_and_unbinds_without_saving() {
    // Plan-layout-backup rules 1b and 5: a kill elsewhere, or a host reboot,
    // removed the token file.
    let world = World::new(Host::Local);
    activate(&world.ports(), &Host::Local, &workspace("4", 4), None).unwrap();
    world.host.state.lock().unwrap().tokens.clear();

    let detached = detach(&world.ports(), "4", 0).unwrap();

    assert_eq!(detached.token, TokenMatch::Ended);
    assert_eq!(world.host.file(SAVED_WS4), None);
    assert!(world.host.state.lock().unwrap().detach_calls.is_empty());
    assert_eq!(world.window_numbers("ws4"), Vec::<u32>::new());
    assert_eq!(world.binding("4"), None);
    assert_eq!(world.host_token("ws4"), None, "detach never takes");
}

#[test]
fn detach_removes_the_backup_whatever_the_token() {
    // Plan-layout-backup rule 16, on all three paths of rule 5.
    let changes: [fn(&World); 3] = [
        |_| {},
        |w| {
            w.take_elsewhere("ws4");
        },
        |w| w.host.state.lock().unwrap().tokens.clear(),
    ];
    for change_token in changes {
        let world = World::new(Host::Local);
        activate(&world.ports(), &Host::Local, &workspace("4", 4), None).unwrap();
        let binding = world.binding("4").unwrap();
        world.backups.write(&binding, &hsplit("ws4", &[1])).unwrap();
        change_token(&world);

        detach(&world.ports(), "4", 0).unwrap();

        assert!(!world.has_backup_file("ws4"));
    }
}

// --------------------------------------------------------------- attach

#[test]
fn attach_takes_the_session_and_binds_its_token() {
    // Plan-layout-backup rule 1.
    let world = World::new(Host::Local);
    world.save_layout("ws4", &[1]);
    let before = world.take_elsewhere("ws4");

    attach(&world.ports(), &Host::Local, "4", None, false).unwrap();

    let token = world.bound_token("4").unwrap();
    assert_ne!(token, before);
    assert_eq!(world.host_token("ws4").as_deref(), Some(token.as_str()));
}

#[test]
fn refused_attach_never_takes() {
    // Plan-layout-backup rule 2: every refusal comes before the take.
    type Setup = fn(&World);
    let refusals: [(&str, Option<&str>, Setup); 6] = [
        ("attached elsewhere", None, |w| {
            w.host.add_shell("ws4-001", true);
        }),
        ("workspace has terminals", None, |w| {
            w.sway.open_marked("4", &w.mark("other-001"));
        }),
        ("session bound elsewhere", Some("ws4"), |w| {
            w.bind("9", "ws4");
        }),
        ("workspace taken", Some("ws4"), |w| w.bind("4", "ws7")),
        ("no such saved session", Some("nope"), |_| {}),
        ("several to choose from", None, |w| {
            w.save_layout("ws7", &[1]);
        }),
    ];
    for (refusal, session, setup) in refusals {
        let world = World::new(Host::Local);
        world.save_layout("ws4", &[1]);
        let token = world.take_elsewhere("ws4");
        setup(&world);
        let before = world.store.load().unwrap();

        let outcome = attach(&world.ports(), &Host::Local, "4", session.map(name), false);

        assert!(
            !matches!(outcome, Ok(AttachOutcome::Attached(_))),
            "{refusal}: {outcome:?}"
        );
        assert_eq!(world.takes(), 1, "{refusal}");
        assert_eq!(world.host_token("ws4").as_deref(), Some(token.as_str()));
        assert_eq!(world.store.load().unwrap(), before, "{refusal}");
    }
}

#[test]
fn attach_whose_take_fails_binds_nothing() {
    let world = World::new(Host::Local);
    world.save_layout("ws4", &[1]);
    world.host.state.lock().unwrap().fail_writes = true;

    let err = attach(&world.ports(), &Host::Local, "4", None, false).unwrap_err();

    assert!(err.to_string().contains("No space left"), "{err:#}");
    assert!(world.store.load().unwrap().get("4").is_none());
    assert_eq!(world.sway.window_count(), 0);
}

#[test]
fn attach_restores_layout_and_binds() {
    let world = World::new(Host::Local);
    world.save_layout("ws4", &[1, 2]);
    world.host.add_shell("ws4-001", false);
    world.host.add_shell("ws4-002", false);

    let outcome = attach(&world.ports(), &Host::Local, "4", None, false).unwrap();

    let AttachOutcome::Attached(attached) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(
        (attached.session, attached.total, attached.recreated),
        (name("ws4"), 2, 0)
    );
    assert_eq!(world.window_numbers("ws4"), [1, 2]);
    assert!(world.sway.commands().contains(&"split h".to_owned()));
    assert_eq!(
        world.store.load().unwrap().get("4").unwrap().session(),
        &name("ws4")
    );
}

#[test]
fn attach_reports_shells_recreated_after_host_loss() {
    // Plan rule 11: the host lost ws4-002 and ws4-003 (for example a reboot).
    let world = World::new(Host::Local);
    world.save_layout("ws4", &[1, 2, 3]);
    world.host.add_shell("ws4-001", false);
    let outcome = attach(&world.ports(), &Host::Local, "4", Some(name("ws4")), false).unwrap();
    let AttachOutcome::Attached(attached) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!((attached.total, attached.recreated), (3, 2));
}

#[test]
fn attach_refuses_a_shell_attached_elsewhere() {
    // Plan rule 10.
    let world = World::new(Host::Local);
    world.save_layout("ws4", &[1, 2]);
    world.host.add_shell("ws4-001", false);
    world.host.add_shell("ws4-002", true);

    let err = attach(&world.ports(), &Host::Local, "4", None, false).unwrap_err();

    assert_eq!(
        err.to_string(),
        "Session 'ws4-002' is already attached from another terminal or host; use --force to take over"
    );
    assert!(world.store.load().unwrap().get("4").is_none());
    assert_eq!(world.sway.window_count(), 0);

    attach(&world.ports(), &Host::Local, "4", None, true).unwrap();
    let launches = world.launcher.argvs();
    assert!(launches.iter().all(|argv| argv.contains(&"-f".to_owned())));
}

#[test]
fn attach_refuses_a_workspace_with_i3mux_windows() {
    // Plan rule 6.
    let world = World::new(Host::Local);
    world.save_layout("ws4", &[1]);
    world.sway.open_marked("4", &world.mark("other-001"));
    let err = attach(&world.ports(), &Host::Local, "4", None, false).unwrap_err();
    assert!(matches!(command_error(&err), CommandError::WorkspaceHasTerminals(ws) if ws == "4"));
    assert!(world.store.load().unwrap().get("4").is_none());
}

#[test]
fn interrupted_attach_leaves_the_workspace_bound() {
    // Plan rule 6: bind before spawning, so detach/kill can clean up.
    let mut world = World::new(Host::Local);
    world.launcher.fail_after = Some(1);
    world.save_layout("ws4", &[1, 2]);
    assert!(attach(&world.ports(), &Host::Local, "4", None, false).is_err());
    assert_eq!(
        world.store.load().unwrap().get("4").unwrap().session(),
        &name("ws4")
    );
    assert_eq!(world.window_numbers("ws4"), [1]);
}

#[test]
fn attach_asks_to_choose_among_several_sessions() {
    let world = World::new(Host::Local);
    world.save_layout("ws4", &[1]);
    world.save_layout("ws7", &[1]);
    world.save_layout("ws9", &[1]);
    world.bind("9", "ws9");
    let outcome = attach(&world.ports(), &Host::Local, "4", None, false).unwrap();
    let AttachOutcome::Choose(candidates) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(
        candidates,
        [name("ws4"), name("ws7")],
        "sessions bound here are not offered"
    );
}

#[test]
fn attach_unknown_session_is_an_error() {
    let world = World::new(Host::Local);
    let err = attach(&world.ports(), &Host::Local, "4", Some(name("nope")), false).unwrap_err();
    assert!(matches!(
        command_error(&err),
        CommandError::NoSavedSession { .. }
    ));
    let err = attach(&world.ports(), &Host::Local, "4", None, false).unwrap_err();
    assert!(matches!(
        command_error(&err),
        CommandError::NoSavedSessions(_)
    ));
}

// --------------------------------------------------------------- resume
// A crashed binding: this machine took the session in an earlier Sway, which
// died with its windows. `World::crashed` builds one.

#[test]
fn terminal_resumes_a_crashed_binding_from_its_backup() {
    // Plan-layout-backup rules 11 and 12.
    let world = World::new(Host::Local);
    let binding = world.crashed("4", "ws4", Some(&[1, 2]));
    // The host still shows the dead window's client on ws4-001, and lost
    // ws4-002.
    world.host.add_shell("ws4-001", true);

    let outcome = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    let TerminalOutcome::Restored(attached) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(attached.source, LayoutSource::Backup);
    assert_eq!((attached.total, attached.recreated), (2, 1));
    assert_eq!(world.window_numbers("ws4"), [1, 2]);
    assert!(world.sway.commands().contains(&"split h".to_owned()));
    assert!(
        world
            .launcher
            .argvs()
            .iter()
            .all(|a| a.contains(&"-f".to_owned())),
        "the same token proves the attached clients are this binding's own"
    );
    let now = world.binding("4").unwrap();
    assert!(now.is_owned_by(&world.current_sway));
    assert_eq!(now.token(), binding.token());
    assert_eq!(world.takes(), 1, "a resume never takes (rule 3)");
    assert_eq!(world.backup(&now), Some(hsplit("ws4", &[1, 2])));
    assert_eq!(
        format_attached(&attached, "4"),
        [
            "✓ Resumed session 'ws4' on local in workspace 4 (2 terminals)",
            "  recreated 1 of 2 shells: the host had lost them, for example after a reboot"
        ]
    );
}

#[test]
fn attach_resumes_a_crashed_binding_that_it_names_or_leaves_open() {
    // Plan-layout-backup rule 11.
    for requested in [None, Some(name("ws4"))] {
        let world = World::new(Host::Local);
        world.crashed("4", "ws4", Some(&[1]));
        let outcome = attach(&world.ports(), &Host::Local, "4", requested, false).unwrap();
        let AttachOutcome::Attached(attached) = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(attached.source, LayoutSource::Backup);
        assert_eq!(world.window_numbers("ws4"), [1]);
    }
}

#[test]
fn attach_naming_another_session_does_not_resume() {
    let world = World::new(Host::Local);
    let binding = world.crashed("4", "ws4", Some(&[1]));
    world.save_layout("ws7", &[1]);

    let err = attach(&world.ports(), &Host::Local, "4", Some(name("ws7")), false).unwrap_err();

    assert!(
        err.to_string().contains("is bound to session 'ws4'"),
        "{err:#}"
    );
    assert_eq!(world.binding("4"), Some(binding));
    assert_eq!(world.sway.window_count(), 0);
}

#[test]
fn an_interrupted_resume_leaves_the_binding_and_the_backup() {
    // Plan-layout-backup rule 12: the owner comes only after the last window.
    let mut world = World::new(Host::Local);
    world.launcher.fail_after = Some(1);
    let binding = world.crashed("4", "ws4", Some(&[1, 2]));

    assert!(terminal(&world.ports(), "4", &TerminalRequest::default()).is_err());

    assert_eq!(world.binding("4"), Some(binding.clone()));
    assert_eq!(world.backup(&binding), Some(hsplit("ws4", &[1, 2])));
}

#[test]
fn resume_after_another_take_uses_the_host_layout() {
    // Plan-layout-backup rule 13.
    let world = World::new(Host::Local);
    let binding = world.crashed("4", "ws4", Some(&[1, 2]));
    world.save_layout("ws4", &[3]);
    let theirs = world.take_elsewhere("ws4");

    let outcome = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    let TerminalOutcome::Restored(attached) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(
        attached.source,
        LayoutSource::HostAfterLoss(Lost::TakenElsewhere)
    );
    assert_eq!(world.window_numbers("ws4"), [3]);
    assert!(
        !world.has_backup_file("ws4"),
        "the backup goes with the binding"
    );
    let now = world.binding("4").unwrap();
    assert_ne!(now.token(), binding.token());
    assert_ne!(
        now.token(),
        Some(&theirs),
        "attach from the host layout takes"
    );
    assert!(now.is_owned_by(&world.current_sway));
    assert_eq!(
        format_attached(&attached, "4")[0],
        "ws4 was taken by another machine since; using the layout saved there."
    );
}

#[test]
fn resume_after_another_take_does_not_force_their_shells() {
    // Plan-layout-backup rule 13: the host layout path is attach as today,
    // so a shell attached elsewhere needs --force.
    let world = World::new(Host::Local);
    world.crashed("4", "ws4", Some(&[1]));
    world.save_layout("ws4", &[1]);
    world.take_elsewhere("ws4");
    world.host.add_shell("ws4-001", true);

    let err = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap_err();

    let message = format!("{err:#}");
    assert!(
        message.contains("taken by another machine since"),
        "{message}"
    );
    assert!(message.contains("use --force"), "{message}");
    assert_eq!(world.binding("4"), None, "the binding is dropped first");
    assert!(!world.has_backup_file("ws4"));
}

#[test]
fn resume_after_the_session_ended_uses_the_host_layout() {
    // Plan-layout-backup rule 13a: a kill elsewhere, or a host reboot.
    let world = World::new(Host::Local);
    world.crashed("4", "ws4", Some(&[1, 2]));
    world.save_layout("ws4", &[3]);
    world.host.state.lock().unwrap().tokens.clear();

    let outcome = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    let TerminalOutcome::Restored(attached) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(attached.source, LayoutSource::HostAfterLoss(Lost::Ended));
    assert_eq!(world.window_numbers("ws4"), [3]);
    assert!(!world.has_backup_file("ws4"));
    assert_eq!(format_attached(&attached, "4")[0], "ws4 ended on local.");
    assert!(
        world.host_token("ws4").is_some(),
        "attach from the host layout takes"
    );
}

#[test]
fn resume_of_an_ended_session_without_a_host_layout_says_so() {
    let world = World::new(Host::Local);
    world.crashed("4", "ws4", Some(&[1]));
    world.host.state.lock().unwrap().tokens.clear();

    let err = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap_err();

    let message = format!("{err:#}");
    assert!(
        message.starts_with("ws4 ended on local. Workspace 4 is no longer bound"),
        "{message}"
    );
    assert!(message.contains("No saved session 'ws4'"), "{message}");
    assert_eq!(world.binding("4"), None);
}

#[test]
fn resume_on_an_unreachable_host_changes_nothing() {
    // Plan-layout-backup rule 14.
    let world = World::new(deepthought());
    let binding = world.crashed("4", "ws4", Some(&[1]));
    world.host.state.lock().unwrap().unreachable = true;

    let err = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap_err();

    assert!(err.to_string().contains("cannot reach"), "{err:#}");
    assert_eq!(world.binding("4"), Some(binding.clone()));
    assert_eq!(world.backup(&binding), Some(hsplit("ws4", &[1])));
    assert_eq!(world.sway.window_count(), 0);
}

#[test]
fn resume_with_an_unreadable_token_changes_nothing() {
    // Plan-layout-backup rule 14.
    let world = World::new(Host::Local);
    let binding = world.crashed("4", "ws4", Some(&[1]));
    let mut host = world.host.state.lock().unwrap();
    host.tokens.insert("ws4".into(), "not a token\n".into());
    drop(host);

    let err = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap_err();

    assert!(err.to_string().contains("bad token"), "{err:#}");
    assert_eq!(world.binding("4"), Some(binding.clone()));
    assert_eq!(world.backup(&binding), Some(hsplit("ws4", &[1])));
}

#[test]
fn resume_without_a_backup_restores_the_host_layout() {
    // Plan-layout-backup rule 15, with force: the token is still ours.
    let world = World::new(Host::Local);
    world.crashed("4", "ws4", None);
    world.save_layout("ws4", &[1, 2]);
    world.host.add_shell("ws4-001", true);

    let outcome = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    let TerminalOutcome::Restored(attached) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(attached.source, LayoutSource::Host);
    assert_eq!(world.window_numbers("ws4"), [1, 2]);
    assert!(world.binding("4").unwrap().is_owned_by(&world.current_sway));
    assert_eq!(world.takes(), 2, "attach from the host layout takes");
}

#[test]
fn resume_without_a_backup_or_host_layout_opens_one_terminal() {
    // A session never detached, after a crash: as before, one terminal on
    // the lowest shell, taken over from the dead window's client.
    let world = World::new(Host::Local);
    world.crashed("4", "ws4", None);
    world.host.add_shell("ws4-001", true);
    world.host.add_shell("ws4-002", true);

    let outcome = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    assert!(
        matches!(&outcome, TerminalOutcome::Opened(Choice::Reattach(t)) if t.number() == 1),
        "{outcome:?}"
    );
    assert!(world.binding("4").unwrap().is_owned_by(&world.current_sway));
    assert_eq!(world.takes(), 1);
}

#[test]
fn a_backup_with_another_token_is_not_restored() {
    // Plan-layout-backup rule 9.
    let world = World::new(Host::Local);
    let binding = world.crashed("4", "ws4", None);
    let stale = WorkspaceState::new(
        Host::Local,
        name("ws4"),
        Token::parse(&"e".repeat(32)).unwrap(),
        earlier_sway(),
    );
    world.backups.write(&stale, &hsplit("ws4", &[7])).unwrap();
    world.save_layout("ws4", &[1]);

    terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    assert_eq!(world.window_numbers("ws4"), [1], "the host layout, not 7");
    assert_eq!(world.backup(&binding), None);
}

#[test]
fn an_unparseable_backup_never_touches_state_json() {
    let world = World::new(Host::Local);
    world.crashed("4", "ws4", None);
    let file = world.dir.path().join("backups/local/ws4.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "{").unwrap();
    world.host.add_shell("ws4-001", false);

    terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    assert!(!world.dir.path().join("state.json.corrupt").exists());
    assert!(world.binding("4").unwrap().is_owned_by(&world.current_sway));
    assert_eq!(world.window_numbers("ws4"), [1]);
}

#[test]
fn a_binding_from_before_tokens_resumes_while_the_host_has_none() {
    // Plan-layout-backup rules 4 and 19: no token matches no token file.
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    let legacy = world.binding("4").unwrap();
    world
        .backups
        .write(&legacy, &hsplit("ws4", &[1, 2]))
        .unwrap();

    let outcome = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    assert!(
        matches!(&outcome, TerminalOutcome::Restored(a) if a.source == LayoutSource::Backup),
        "{outcome:?}"
    );
    assert_eq!(world.window_numbers("ws4"), [1, 2]);
    assert_eq!(world.takes(), 0);
}

#[test]
fn a_binding_whose_windows_are_here_is_adopted_not_resumed() {
    // After an upgrade, live bindings have no owner. Their windows are in
    // this Sway, so it owns them (plan-layout-backup decision 2).
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.sway.open_marked("4", &world.mark("ws4-001"));
    world.host.add_shell("ws4-001", true);

    let outcome = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    assert!(
        matches!(&outcome, TerminalOutcome::Opened(Choice::Create(t)) if t.number() == 2),
        "{outcome:?}"
    );
    assert!(world.binding("4").unwrap().is_owned_by(&world.current_sway));
    assert_eq!(world.takes(), 0, "no host token read or written");
}

#[test]
fn resume_refuses_a_workspace_with_other_i3mux_windows() {
    // Plan rule 6, before any host contact.
    let world = World::new(Host::Local);
    let binding = world.crashed("4", "ws4", Some(&[1]));
    world.sway.open_marked("4", &world.mark("other-001"));

    let err = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap_err();

    assert!(matches!(
        command_error(&err),
        CommandError::WorkspaceHasTerminals(_)
    ));
    assert_eq!(world.binding("4"), Some(binding));
}

#[test]
fn terminal_in_a_binding_of_this_sway_opens_one_window() {
    // Not resumable: this Sway owns it, even with no window left.
    let world = World::new(Host::Local);
    activate(&world.ports(), &Host::Local, &workspace("4", 4), None).unwrap();
    let binding = world.binding("4").unwrap();
    world
        .backups
        .write(&binding, &hsplit("ws4", &[1, 2, 3]))
        .unwrap();

    let outcome = terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    assert!(matches!(outcome, TerminalOutcome::Opened(_)), "{outcome:?}");
    assert_eq!(world.window_numbers("ws4"), [1, 2]);
}

// ----------------------------------------------------------------- kill

#[test]
fn kill_ends_windows_shells_layout_and_binding() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.save_layout("ws4", &[1]);
    world.host.add_shell("ws4-001", true);
    world.host.add_shell("ws4-002", false);
    world.host.add_shell("ws4-x-001", false);
    world.sway.open_marked("4", &world.mark("ws4-001"));

    let killed = kill(&world.ports(), &Host::Local, &name("ws4")).unwrap();

    assert_eq!(
        (killed.windows, killed.shells, killed.layout_deleted),
        (1, 2, true)
    );
    assert_eq!(
        world.host.shells().keys().collect::<Vec<_>>(),
        ["ws4-x-001"]
    );
    assert_eq!(world.host.file(SAVED_WS4), None);
    assert!(world.store.load().unwrap().get("4").is_none());
    assert_eq!(world.window_numbers("ws4"), Vec::<u32>::new());
}

#[test]
fn kill_ends_the_session() {
    // Plan-layout-backup rule 1b: a binding of the killed session on another
    // machine finds it ended, so its detach cannot save the layout again.
    let world = World::new(Host::Local);
    world.save_layout("ws4", &[1]);
    let other_machine = world.take_elsewhere("ws4");

    kill(&world.ports(), &Host::Local, &name("ws4")).unwrap();

    assert_eq!(world.host_token("ws4"), None);
    let saved = SavedSession::new(name("ws4"), Host::Local, hsplit("ws4", &[1]), 0).unwrap();
    assert_eq!(
        session::save_if_token(&world.host, &saved, Some(&other_machine)).unwrap(),
        TokenMatch::Ended
    );
    assert_eq!(world.host.file(SAVED_WS4), None);
}

#[test]
fn kill_stopped_halfway_has_taken_the_session() {
    // Plan-layout-backup rule 1: the take comes first, so other machines'
    // bindings no longer hold the session even if the kill fails later.
    let world = World::new(Host::Local);
    world.save_layout("ws4", &[1]);
    let other_machine = world.take_elsewhere("ws4");
    world.host.state.lock().unwrap().remove_error = Some("Permission denied");

    assert!(kill(&world.ports(), &Host::Local, &name("ws4")).is_err());

    let token = world.host_token("ws4").unwrap();
    assert_ne!(token, other_machine.as_str());
}

#[test]
fn kill_of_an_unknown_session_leaves_no_token_file() {
    // Plan-layout-backup rule 1b.
    let world = World::new(Host::Local);
    let err = kill(&world.ports(), &Host::Local, &name("ws4")).unwrap_err();
    assert!(matches!(
        command_error(&err),
        CommandError::NoSuchSession { .. }
    ));
    assert_eq!(world.host_token("ws4"), None);
}

#[test]
fn kill_removes_the_backup() {
    // Plan-layout-backup rule 16.
    let world = World::new(Host::Local);
    world.crashed("4", "ws4", Some(&[1]));
    assert!(world.has_backup_file("ws4"));

    kill(&world.ports(), &Host::Local, &name("ws4")).unwrap();

    assert!(!world.has_backup_file("ws4"));
    assert_eq!(world.binding("4"), None);
}

#[test]
fn kill_whose_take_fails_leaves_everything() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.save_layout("ws4", &[1]);
    world.sway.open_marked("4", &world.mark("ws4-001"));
    world.host.add_shell("ws4-001", true);
    world.host.state.lock().unwrap().fail_writes = true;

    assert!(kill(&world.ports(), &Host::Local, &name("ws4")).is_err());

    assert_eq!(world.window_numbers("ws4"), [1]);
    assert_eq!(world.host.shells().len(), 1);
    assert!(world.host.file(SAVED_WS4).is_some());
    assert!(world.store.load().unwrap().get("4").is_some());
}

#[test]
fn kill_on_unreachable_host_leaves_windows_open() {
    let remote = Host::Ssh(RemoteHost::new("deepthought").unwrap());
    let world = World::new(remote.clone());
    world.bind("4", "ws4");
    world.sway.open_marked("4", &world.mark("ws4-001"));
    world.host.state.lock().unwrap().unreachable = true;

    let err = world
        .finish(|ports| kill(ports, &remote, &name("ws4")))
        .unwrap_err();

    assert!(err.to_string().contains("cannot reach"), "{err:#}");
    assert!(
        world.sway.commands().is_empty(),
        "{:?}",
        world.sway.commands()
    );
    assert_eq!(world.window_numbers("ws4"), [1]);
    assert!(world.store.load().unwrap().get("4").is_some());
    assert!(world.fakes.stopped().is_empty(), "ws4 is still bound");
    assert_eq!(world.takes(), 0);
}

#[test]
fn kill_unknown_session_is_an_error() {
    let world = World::new(Host::Local);
    let err = kill(&world.ports(), &Host::Local, &name("ws4")).unwrap_err();
    assert!(matches!(
        command_error(&err),
        CommandError::NoSuchSession { .. }
    ));
}

// ------------------------------------------------------------- sessions

#[test]
fn sessions_lists_bound_detached_and_running() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.sway.open_marked("4", &world.mark("ws4-001"));
    world.sway.open_marked("4", &world.mark("ws4-002"));
    world.host.add_shell("ws4-001", true);
    world.host.add_shell("ws4-002", true);
    world.host.add_shell("ws4-003", false);
    world.save_layout("ws7", &[1, 2]);
    world.host.add_shell("ws10-001", false);

    let rows = sessions(&world.ports(), &Host::Local).unwrap();

    assert_eq!(
        format_rows(&rows),
        [
            "ws10  (local)  1 shell, no saved layout",
            "ws4   (local)  workspace 4, 2 windows, 1 orphan",
            "ws7   (local)  detached, 2 terminals saved",
        ]
    );
}

#[test]
fn sessions_reports_a_session_attached_on_another_machine() {
    // Machine A detached ws4, then attached it again; this is machine B.
    let world = World::new(Host::Local);
    world.save_layout("ws4", &[1, 2]);
    world.host.add_shell("ws4-001", true);
    world.host.add_shell("ws4-002", false);

    let rows = sessions(&world.ports(), &Host::Local).unwrap();

    assert_eq!(
        format_rows(&rows),
        ["ws4  (local)  attached elsewhere, 2 shells"]
    );
}

#[test]
fn one_corrupt_saved_layout_does_not_hide_the_others() {
    let world = World::new(Host::Local);
    world.save_layout("ws7", &[1]);
    world
        .host
        .state
        .lock()
        .unwrap()
        .files
        .insert(".local/state/i3mux/sessions/ws9.json".into(), "{".into());

    let rows = sessions(&world.ports(), &Host::Local).unwrap();

    assert_eq!(
        format_rows(&rows),
        [
            "ws7  (local)  detached, 1 terminal saved",
            "ws9  (local)  unreadable saved layout",
        ]
    );
}

#[test]
fn sessions_row_wording() {
    let row = |status| SessionRow {
        name: name("ws4"),
        host: Host::Ssh(RemoteHost::new("deepthought").unwrap()),
        status,
    };
    let rows = [
        row(RowStatus::Bound {
            workspace: "4".into(),
            windows: 3,
            orphans: 1,
        }),
        row(RowStatus::Bound {
            workspace: "mail".into(),
            windows: 1,
            orphans: 0,
        }),
        row(RowStatus::Detached { saved: 1 }),
    ];
    assert_eq!(
        format_rows(&rows),
        [
            "ws4  (deepthought)  workspace 4, 3 windows, 1 orphan",
            "ws4  (deepthought)  workspace mail, 1 window",
            "ws4  (deepthought)  detached, 1 terminal saved",
        ]
    );
}

/// A world on a remote host, plus a second fake host for this machine,
/// so `sessions_all` can ask both.
fn with_local_host(world: &World) -> (FakeHost, Box<HostFactory>) {
    let local = FakeHost::new(Host::Local, world.masters.clone());
    let (remote, local_for_factory) = (world.host.clone(), local.clone());
    let hosts: Box<HostFactory> = Box::new(move |h: &Host| -> Box<dyn HostExec> {
        if h == &Host::Local {
            Box::new(local_for_factory.clone())
        } else {
            assert_eq!(h, remote.host(), "unexpected host {h}");
            Box::new(remote.clone())
        }
    });
    (local, hosts)
}

fn deepthought() -> Host {
    Host::Ssh(RemoteHost::new("deepthought").unwrap())
}

#[test]
fn sessions_all_lists_this_machine_and_every_bound_host() {
    // The bug: the menu listed only this machine and the last host typed,
    // so a bound remote session vanished when that host was mistyped.
    let world = World::new(deepthought());
    world.bind("3", "ws3");
    world.sway.open_marked("3", &world.mark("ws3-001"));
    world.host.add_shell("ws3-001", true);
    let (local, hosts) = with_local_host(&world);
    local.add_shell("ws5-001", false);
    let ports = Ports {
        hosts: hosts.as_ref(),
        ..world.ports()
    };

    let listing = sessions_all(&ports, None).unwrap();

    assert_eq!(listing.listed, [Host::Local, deepthought()]);
    assert!(listing.failed.is_empty());
    assert_eq!(
        format_rows(&listing.rows),
        [
            "ws5  (local)  1 shell, no saved layout",
            "ws3  (deepthought)  workspace 3, 1 window",
        ]
    );
}

#[test]
fn sessions_all_asks_the_extra_host_once() {
    let world = World::new(deepthought());
    world.bind("3", "ws3");
    let (_local, hosts) = with_local_host(&world);
    let ports = Ports {
        hosts: hosts.as_ref(),
        ..world.ports()
    };
    let listing = sessions_all(&ports, Some(deepthought())).unwrap();
    assert_eq!(listing.listed, [Host::Local, deepthought()]);
}

#[test]
fn sessions_all_survives_an_unreachable_host() {
    let world = World::new(deepthought());
    world.bind("3", "ws3");
    world.host.state.lock().unwrap().unreachable = true;
    let (local, hosts) = with_local_host(&world);
    local.add_shell("ws5-001", false);
    let ports = Ports {
        hosts: hosts.as_ref(),
        ..world.ports()
    };

    let listing = sessions_all(&ports, None).unwrap();

    assert_eq!(
        format_rows(&listing.rows),
        ["ws5  (local)  1 shell, no saved layout"]
    );
    assert_eq!(listing.failed.len(), 1);
    assert_eq!(listing.failed[0].0, deepthought());
}

// ---------------------------------------------------------- ssh masters

fn dt() -> RemoteHost {
    RemoteHost::new("deepthought").unwrap()
}

#[test]
fn remote_window_goes_through_the_hosts_master() {
    // Plan-ssh-recovery rules 1 and 8: the master runs before the window
    // opens, and the window knows its unit and how to reattach with --force.
    let world = World::new(deepthought());
    world.bind("4", "ws4");

    world
        .finish(|ports| open_terminal(ports, "4", &TerminalRequest::default()))
        .unwrap();

    assert!(world.fakes.running(&dt()), "{:?}", world.fakes.events());
    let launches = world.launcher.launches.lock().unwrap();
    let WindowCommand::Remote(remote) = &launches[0] else {
        panic!("{launches:?}")
    };
    assert_eq!(remote.unit, FakeMasters::unit());
    assert_eq!((&remote.host, &remote.shell), (&dt(), &t("ws4", 1)));
    assert_eq!(remote.argv[remote.argv.len() - 2..], ["attach", "ws4-001"]);
    assert_eq!(
        remote.reattach[remote.reattach.len() - 3..],
        ["attach", "-f", "ws4-001"]
    );
}

#[test]
fn local_window_has_no_master() {
    // Plan-ssh-recovery rule 14.
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world
        .finish(|ports| open_terminal(ports, "4", &TerminalRequest::default()))
        .unwrap();
    assert!(world.fakes.events().is_empty());
    let launches = world.launcher.launches.lock().unwrap();
    assert!(
        matches!(launches[0], WindowCommand::Plain(_)),
        "{launches:?}"
    );
}

#[test]
fn remote_window_without_masters_is_a_plain_window() {
    // No systemd user manager: each ssh connects on its own, as before.
    let world = World::new(deepthought());
    world.fakes.log.lock().unwrap().direct = true;
    world.bind("4", "ws4");
    open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();
    let launches = world.launcher.launches.lock().unwrap();
    assert!(
        matches!(launches[0], WindowCommand::Plain(_)),
        "{launches:?}"
    );
}

#[test]
fn no_window_opens_when_the_master_does_not_start() {
    // Plan-ssh-recovery rule 6.
    let world = World::new(deepthought());
    world.fakes.log.lock().unwrap().fail_start = true;
    world.bind("4", "ws4");

    let err = open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap_err();

    assert!(
        err.to_string()
            .ends_with("Log: journalctl --user -u i3mux-ssh-fake.service"),
        "{err:#}"
    );
    assert!(world.launcher.launches.lock().unwrap().is_empty());
    assert_eq!(world.sway.window_count(), 0);
}

// Plan-ssh-recovery rule 12: after each command, the masters of the hosts it
// touched that no workspace uses stop. `running() == false` after a stop also
// shows that nothing ran on the host after the stop.

#[test]
fn detach_of_the_last_workspace_on_a_host_stops_its_master() {
    let world = World::new(deepthought());
    world.bind("4", "ws4");
    world.host.state.lock().unwrap().detach_lag = 2;
    world
        .finish(|ports| open_terminal(ports, "4", &TerminalRequest::default()))
        .unwrap();
    assert!(world.fakes.running(&dt()));

    world.finish(|ports| detach(ports, "4", 0)).unwrap();

    assert_eq!(world.fakes.stopped(), [dt()]);
    assert!(!world.fakes.running(&dt()), "{:?}", world.fakes.events());
}

#[test]
fn a_master_stays_while_another_workspace_uses_the_host() {
    let world = World::new(deepthought());
    world.bind("4", "ws4");
    world.bind("5", "ws5");
    world.host.add_shell("ws4-001", false);
    open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    world.finish(|ports| detach(ports, "4", 0)).unwrap();
    world
        .finish(|ports| kill(ports, &deepthought(), &name("ws4")))
        .unwrap();

    assert!(world.fakes.stopped().is_empty(), "ws5 is still bound");
    assert!(world.fakes.running(&dt()));
}

#[test]
fn kill_of_the_last_session_on_a_host_stops_its_master() {
    let world = World::new(deepthought());
    world.bind("4", "ws4");
    world.sway.open_marked("4", &world.mark("ws4-001"));
    world.host.add_shell("ws4-001", true);

    world
        .finish(|ports| kill(ports, &deepthought(), &name("ws4")))
        .unwrap();

    assert_eq!(world.fakes.stopped(), [dt()]);
    assert!(!world.fakes.running(&dt()));
}

#[test]
fn failed_attach_leaves_no_master_running() {
    // The command fails, and its error is still the one the caller sees.
    let world = World::new(deepthought());

    let err = world
        .finish(|ports| attach(ports, &deepthought(), "4", Some(name("nope")), false))
        .unwrap_err();

    assert!(matches!(
        command_error(&err),
        CommandError::NoSavedSession { .. }
    ));
    assert!(!world.fakes.ensured().is_empty(), "attach asked the host");
    assert_eq!(world.fakes.stopped(), [dt()]);
    assert!(!world.fakes.running(&dt()));
}

#[test]
fn sessions_of_an_unbound_host_stop_its_master() {
    let world = World::new(deepthought());
    world.host.add_shell("ws4-001", false);

    let rows = world
        .finish(|ports| sessions(ports, &deepthought()))
        .unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(world.fakes.stopped(), [dt()]);
    assert!(!world.fakes.running(&dt()));
}

#[test]
fn a_master_stays_only_for_bindings_of_this_sway() {
    // Plan-layout-backup rule 18. A binding from before owners keeps its
    // master: its windows may be in this Sway.
    let world = World::new(deepthought());
    world.crashed("4", "ws4", None);
    world
        .finish(|ports| sessions(ports, &deepthought()))
        .unwrap();
    assert_eq!(
        world.fakes.stopped(),
        [dt()],
        "the crashed binding has no windows"
    );

    let world = World::new(deepthought());
    world.crashed("4", "ws4", None);
    world.bind("5", "ws5");
    world
        .finish(|ports| sessions(ports, &deepthought()))
        .unwrap();
    assert!(world.fakes.stopped().is_empty());

    let world = World::new(deepthought());
    world.crashed("4", "ws4", None);
    activate(&world.ports(), &deepthought(), &workspace("5", 5), None).unwrap();
    world
        .finish(|ports| sessions(ports, &deepthought()))
        .unwrap();
    assert!(world.fakes.stopped().is_empty());
}

#[test]
fn only_masters_the_command_touched_are_stopped() {
    // A local command touches no remote host, so it stops nothing, even
    // with no workspace bound anywhere.
    let world = World::new(Host::Local);
    world.finish(|ports| sessions(ports, &Host::Local)).unwrap();
    assert!(world.fakes.events().is_empty());
}

// --------------------------------------------------------------- status

#[test]
fn status_shows_the_focused_session_without_asking_the_host() {
    let world = World::new(deepthought());
    world.bind("3", "ws3");
    world.sway.open_marked("3", &world.mark("ws3-001"));
    world.sway.open_marked("3", &world.mark("ws3-002"));
    world.host.state.lock().unwrap().unreachable = true;

    let report = world.finish(|ports| status(ports, "3")).unwrap().unwrap();

    // Plan-ssh-recovery rule 13: the master's state comes from systemd.
    assert_eq!(
        format_status(&report),
        [
            "ws3  (deepthought)  workspace 3, 2 windows",
            "  connection: active, 2 restarts (i3mux-ssh-fake.service)",
        ]
    );
    assert!(
        world.fakes.events().is_empty(),
        "status starts and stops nothing"
    );
}

#[test]
fn status_of_a_local_session_has_no_connection() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    let report = status(&world.ports(), "4").unwrap().unwrap();
    assert_eq!(
        format_status(&report),
        ["ws4  (local)  workspace 4, 0 windows"]
    );
}

#[test]
fn status_of_an_unbound_workspace_is_none() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    assert!(status(&world.ports(), "7").unwrap().is_none());
}

// ------------------------------------------------------------------ cli

#[test]
fn cli_definition_is_valid() {
    use clap::CommandFactory;
    Cli::command().debug_assert();
}

#[test]
fn kill_remote_requires_session() {
    assert!(Cli::try_parse_from(["i3mux", "kill", "--remote", "h"]).is_err());
    assert!(Cli::try_parse_from(["i3mux", "kill", "--remote", "h", "--session", "s"]).is_ok());
    assert!(Cli::try_parse_from(["i3mux", "kill"]).is_ok());
}
