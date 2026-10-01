//! Tests of the core command functions against fakes: an in-memory host
//! (shells and files), the in-memory Sway, and a launcher that opens fake
//! windows. Each test names the plan rule it backs.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::host::HostError;
use crate::layout::Layout;
use crate::sway_ops::testing::FakeSway;
use crate::types::RemoteHost;

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
/// by the argv tail; everything else succeeds silently.
#[derive(Clone)]
struct FakeHost {
    host: Host,
    state: Arc<Mutex<HostState>>,
}

impl FakeHost {
    fn new(host: Host) -> Self {
        Self {
            host,
            state: Arc::default(),
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

    fn check_reachable(&self) -> Result<(), HostError> {
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
        self.check_reachable()?;
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

    fn write_file_atomic(&self, path: &str, content: &str) -> Result<(), HostError> {
        self.check_reachable()?;
        let mut state = self.state.lock().unwrap();
        if state.fail_writes {
            return Err(self.failure("No space left on device"));
        }
        state.files.insert(path.to_owned(), content.to_owned());
        Ok(())
    }

    fn read_file(&self, path: &str) -> Result<Option<String>, HostError> {
        self.check_reachable()?;
        Ok(self.file(path))
    }

    fn list_dir(&self, dir: &str) -> Result<Vec<String>, HostError> {
        self.check_reachable()?;
        let prefix = format!("{dir}/");
        let state = self.state.lock().unwrap();
        Ok(state
            .files
            .keys()
            .filter_map(|p| p.strip_prefix(&prefix).map(str::to_owned))
            .collect())
    }

    fn remove_file(&self, path: &str) -> Result<bool, HostError> {
        self.check_reachable()?;
        Ok(self.state.lock().unwrap().files.remove(path).is_some())
    }
}

/// Opens a window on the fake Sway (on `workspace`) and, like `shpool
/// attach`, creates or attaches the shell on the fake host. Can be told to
/// fail after some launches.
struct FakeLauncher {
    sway: Arc<FakeSway>,
    host: FakeHost,
    workspace: String,
    launches: Mutex<Vec<Vec<String>>>,
    fail_after: Option<usize>,
}

impl Launcher for FakeLauncher {
    fn spawn(&self, app_id: &str, _title: &str, argv: &[String]) -> Result<()> {
        let mut launches = self.launches.lock().unwrap();
        if self.fail_after.is_some_and(|n| launches.len() >= n) {
            anyhow::bail!("terminal crashed");
        }
        launches.push(argv.to_vec());
        let mark = Mark::parse(app_id).unwrap();
        self.host.add_shell(&mark.terminal().to_string(), true);
        self.sway.open(&self.workspace, app_id);
        Ok(())
    }
}

/// One test world: a state store in a temp dir, a fake Sway, a fake host
/// (the only host), and a launcher that opens windows on workspace "4".
struct World {
    _dir: tempfile::TempDir,
    store: StateStore,
    sway: Arc<FakeSway>,
    host: FakeHost,
    launcher: FakeLauncher,
    hosts: Box<HostFactory>,
}

impl World {
    fn new(host: Host) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        let sway = Arc::new(FakeSway::default());
        let host = FakeHost::new(host);
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
        Self {
            _dir: dir,
            store,
            sway,
            host,
            launcher,
            hosts,
        }
    }

    fn ports(&self) -> Ports<'_> {
        Ports {
            store: &self.store,
            sway: self.sway.as_ref(),
            launcher: &self.launcher,
            hosts: self.hosts.as_ref(),
            log: Log { verbose: false },
            disconnect_wait: Duration::from_millis(300),
        }
    }

    fn bind(&self, workspace: &str, session: &str) {
        let binding = WorkspaceState::new(self.host.host().clone(), name(session));
        self.store
            .with_lock(|s| Ok(s.bind(workspace, binding)?))
            .unwrap();
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
        let children = numbers
            .iter()
            .map(|n| Layout::Terminal {
                terminal: TerminalName::new(&name(session), *n),
                percent: None,
            })
            .collect();
        let layout = Layout::HSplit {
            children,
            percent: None,
        };
        let saved = SavedSession::new(name(session), self.host.host().clone(), layout, 0).unwrap();
        session::save(&self.host, &saved).unwrap();
    }
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
    let launch = &world.launcher.launches.lock().unwrap()[0];
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
    let launch = &world.launcher.launches.lock().unwrap()[0];
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

// --------------------------------------------------------------- attach

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
    let launches = world.launcher.launches.lock().unwrap();
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
fn kill_on_unreachable_host_leaves_windows_open() {
    let remote = Host::Ssh(RemoteHost::new("deepthought").unwrap());
    let world = World::new(remote.clone());
    world.bind("4", "ws4");
    world.sway.open_marked("4", &world.mark("ws4-001"));
    world.host.state.lock().unwrap().unreachable = true;

    let err = kill(&world.ports(), &remote, &name("ws4")).unwrap_err();

    assert!(err.to_string().contains("cannot reach"), "{err:#}");
    assert!(
        world.sway.commands().is_empty(),
        "{:?}",
        world.sway.commands()
    );
    assert_eq!(world.window_numbers("ws4"), [1]);
    assert!(world.store.load().unwrap().get("4").is_some());
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
    let local = FakeHost::new(Host::Local);
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

// --------------------------------------------------------------- status

#[test]
fn status_shows_the_focused_session_without_asking_the_host() {
    let world = World::new(deepthought());
    world.bind("3", "ws3");
    world.sway.open_marked("3", &world.mark("ws3-001"));
    world.sway.open_marked("3", &world.mark("ws3-002"));
    world.host.state.lock().unwrap().unreachable = true;

    let row = status(&world.ports(), "3").unwrap().unwrap();

    assert_eq!(
        format_rows(&[row]),
        ["ws3  (deepthought)  workspace 3, 2 windows"]
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
