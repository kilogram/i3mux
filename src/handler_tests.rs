//! Tests of the core command functions against fakes: an in-memory host
//! (shells and files), the in-memory Sway, and a launcher that opens fake
//! windows. Each test names the plan rule it backs.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::host::HostError;
use crate::layout::Layout;
use crate::master::{MasterError, TrackedMasters, UnitName};
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
    /// Every shell made with `shpool attach -b`: its name and `-c` command.
    created: Vec<(String, Option<String>)>,
    /// Make `shpool attach -b` fail with this error, as a host whose shpool
    /// cannot start a shell.
    fail_create: Option<String>,
    /// Shells that `shpool attach -b` makes, but that end at once.
    vanishing: Vec<String>,
    /// Like shpool, a shell that `attach -b` made lists as attached for
    /// this many `list` calls, while its client leaves.
    create_lag: usize,
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
        if let Some(at) = argv.iter().position(|a| a == "attach") {
            if argv.get(at + 1).is_some_and(|a| a == "-b") {
                if let Some(stderr) = &state.fail_create {
                    return Err(self.failure(stderr));
                }
                let name = argv.last().cloned().unwrap_or_default();
                let tail = &argv[at..];
                let cmd = tail
                    .iter()
                    .position(|a| a == "-c")
                    .and_then(|c| tail.get(c + 1).cloned());
                if !state.shells.contains_key(&name) && state.create_lag > 0 {
                    let lag = state.create_lag;
                    state.shells.insert(name.clone(), true);
                    state.detaching.insert(name.clone(), lag - 1);
                }
                if !state.vanishing.contains(&name) {
                    state.shells.entry(name.clone()).or_insert(false);
                }
                state.created.push((name, cmd));
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
        self.connect()?;
        let mut state = self.state.lock().unwrap();
        if state.fail_writes {
            return Err(self.failure("No space left on device"));
        }
        state.files.insert(path.to_owned(), content.to_owned());
        Ok(())
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
        Ok(self.state.lock().unwrap().files.remove(path).is_some())
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
    /// The wrapper options of each launch, in order.
    options: Mutex<Vec<WrapperOptions>>,
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
    fn spawn(&self, mark: &Mark, command: &WindowCommand, options: WrapperOptions) -> Result<()> {
        let mut launches = self.launches.lock().unwrap();
        if self.fail_after.is_some_and(|n| launches.len() >= n) {
            anyhow::bail!("terminal crashed");
        }
        launches.push(command.clone());
        self.options.lock().unwrap().push(options);
        self.host.add_shell(&mark.terminal().to_string(), true);
        self.sway.open(&self.workspace, &mark.app_id());
        Ok(())
    }

    /// `[OPTIONS, TITLE, ARGV...]`, so a test sees what reaches the window.
    fn program_argv(
        &self,
        mark: &Mark,
        command: &WindowCommand,
        options: WrapperOptions,
    ) -> Vec<String> {
        let mut argv = vec![format!("{options:?}"), mark.title()];
        argv.extend_from_slice(command.argv());
        argv
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
    /// The masters' log; `masters` wraps it as `run` does.
    fakes: FakeMasters,
    masters: Arc<TrackedMasters>,
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
            options: Mutex::default(),
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
            fakes,
            masters,
        }
    }

    /// Run a core function as `run` runs a command: then stop the masters
    /// that no workspace uses.
    fn finish<T>(&self, command: impl FnOnce(&Ports) -> Result<T>) -> Result<T> {
        with_unused_masters_stopped(&self.store, &self.masters, self.sway.as_ref(), || {
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

// ------------------------------------------------------- window command

fn template(text: &str) -> WindowTemplate {
    WindowTemplate::new(text).unwrap()
}

impl World {
    /// Bind `workspace` to `session` with the window command `command`.
    fn bind_with(&self, workspace: &str, session: &str, command: &str) {
        let binding = WorkspaceState::new(self.host.host().clone(), name(session))
            .with_window_command(Some(template(command)));
        self.store
            .with_lock(|s| Ok(s.bind(workspace, binding)?))
            .unwrap();
    }

    fn created(&self) -> Vec<(String, Option<String>)> {
        self.host.state.lock().unwrap().created.clone()
    }

    fn launches(&self) -> Vec<WindowCommand> {
        self.launcher.launches.lock().unwrap().clone()
    }
}

/// PLAN-terminal-here decision 2: `activate --window-command` stores the
/// command in the binding, makes the first shell without attaching it, and
/// runs the command, with the names, instead of the wrapper.
#[test]
fn activate_runs_the_window_command_in_the_first_window() {
    let world = World::new(Host::Local);
    let (_, choice) = activate(
        &world.ports(),
        &Host::Local,
        &workspace("4", 4),
        None,
        Some(template("my-window --session {session} {shell}")),
    )
    .unwrap();

    assert_eq!(choice, Choice::Create(t("ws4", 1)));
    assert_eq!(world.created(), [("ws4-001".to_owned(), None)]);
    assert_eq!(
        world.launches(),
        [WindowCommand::Custom(
            ["my-window", "--session", "ws4", "ws4-001"]
                .map(str::to_owned)
                .to_vec()
        )]
    );
    assert_eq!(
        world.window_numbers("ws4"),
        [1],
        "the window keeps its mark"
    );
    let binding = world.store.load().unwrap().get("4").cloned().unwrap();
    assert_eq!(
        binding.window_command().unwrap().as_str(),
        "my-window --session {session} {shell}"
    );
}

/// `terminal` runs the window command too: an orphan is shown as it is,
/// and a new shell (here with `-e`) is made first, with its command.
#[test]
fn terminal_runs_the_window_command_and_makes_a_new_shell_first() {
    let world = World::new(Host::Local);
    world.bind_with("4", "ws4", "w {shell}");
    world.host.add_shell("ws4-001", false);

    let orphan = open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();
    let request = TerminalRequest {
        command: Some("htop".into()),
        ..TerminalRequest::default()
    };
    let fresh = open_terminal(&world.ports(), "4", &request).unwrap();

    assert_eq!(orphan, Choice::Reattach(t("ws4", 1)));
    assert_eq!(fresh, Choice::Create(t("ws4", 2)));
    assert_eq!(
        world.created(),
        [("ws4-002".to_owned(), Some("htop".to_owned()))],
        "an orphan is never made again"
    );
    assert_eq!(
        world.launches(),
        [
            WindowCommand::Custom(vec!["w".into(), "ws4-001".into()]),
            WindowCommand::Custom(vec!["w".into(), "ws4-002".into()]),
        ]
    );
}

/// `detach` saves the window command with the layout, and `attach` binds
/// with it again and runs it in every window, making the shells the host
/// lost.
#[test]
fn detach_and_attach_keep_the_window_command() {
    let world = World::new(Host::Local);
    world.bind_with("4", "ws4", "w {shell}");
    world.host.add_shell("ws4-001", true);
    world.sway.open_marked("4", &world.mark("ws4-001"));
    world.host.state.lock().unwrap().detach_lag = 0;
    detach(&world.ports(), "4", 0).unwrap();
    world.host.state.lock().unwrap().shells.clear();

    attach(&world.ports(), &Host::Local, "4", Some(name("ws4")), false).unwrap();

    let binding = world.store.load().unwrap().get("4").cloned().unwrap();
    assert_eq!(binding.window_command().unwrap().as_str(), "w {shell}");
    assert_eq!(world.created(), [("ws4-001".to_owned(), None)]);
    assert_eq!(
        world.launches(),
        [WindowCommand::Custom(vec!["w".into(), "ws4-001".into()])]
    );
}

/// A word of a window command a user might write: spaces, both quotes,
/// `$`, backslashes, globs, non-ASCII text, an empty word, and the
/// placeholders, alone or inside other text.
fn window_word() -> impl proptest::strategy::Strategy<Value = String> {
    use proptest::prelude::*;
    prop_oneof![
        "[a-z/._-]{1,8}",
        Just(String::new()),
        Just("a b".to_owned()),
        Just("it's".to_owned()),
        Just("say \"hi\"".to_owned()),
        Just("$HOME \\n *".to_owned()),
        Just("grüße – ✓".to_owned()),
        Just("{session}".to_owned()),
        Just("{shell}".to_owned()),
        Just("--name={session}/{shell}".to_owned()),
        Just("{x} {SHELL} {shell".to_owned()),
    ]
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

    /// PLAN-terminal-here decision 2 over any command line a shell would
    /// quote: the window command round-trips through state.json, through
    /// `detach` (the saved layout) and `attach`, word for word, and every
    /// window runs exactly its words with the names put in.
    #[test]
    fn a_window_command_with_quotes_and_spaces_round_trips_intact(
        program in "[a-z][a-z-]{0,7}",
        rest in proptest::collection::vec(window_word(), 0..6),
    ) {
        let mut words = vec![program];
        words.extend(rest);
        let text = shell_words::join(&words);
        let world = World::new(Host::Local);
        world.bind_with("4", "ws4", &text);
        let reloaded = world.store.load().unwrap().get("4").cloned().unwrap();
        proptest::prop_assert_eq!(reloaded.window_command().unwrap().as_str(), text.as_str());
        world.host.add_shell("ws4-001", true);
        world.sway.open_marked("4", &world.mark("ws4-001"));
        world.host.state.lock().unwrap().detach_lag = 0;
        detach(&world.ports(), "4", 0).unwrap();
        world.host.state.lock().unwrap().shells.clear();

        attach(&world.ports(), &Host::Local, "4", Some(name("ws4")), false).unwrap();

        let binding = world.store.load().unwrap().get("4").cloned().unwrap();
        proptest::prop_assert_eq!(binding.window_command().unwrap().as_str(), text.as_str());
        let expected: Vec<String> = words
            .iter()
            .map(|w| w.replace("{session}", "ws4").replace("{shell}", "ws4-001"))
            .collect();
        proptest::prop_assert_eq!(world.launches(), vec![WindowCommand::Custom(expected)]);
    }
}

/// `open` shows a shell that another program made through the window
/// command of the workspace bound to its session; the shell is not made
/// again.
#[test]
fn open_runs_the_window_command() {
    let world = World::new(Host::Local);
    world.bind_with("4", "ws4", "w {session} {shell}");
    world.host.add_shell("ws4-007", false);

    open(&world.ports(), &Host::Local, &t("ws4", 7), false).unwrap();

    assert!(world.created().is_empty());
    assert_eq!(
        world.launches(),
        [WindowCommand::Custom(vec![
            "w".into(),
            "ws4".into(),
            "ws4-007".into()
        ])]
    );
}

#[test]
fn status_names_the_window_command() {
    let world = World::new(Host::Local);
    world.bind_with("4", "ws4", "w {shell}");
    let report = status(&world.ports(), "4").unwrap().unwrap();
    assert_eq!(
        format_status(&report),
        [
            "ws4  (local)  workspace 4, 0 windows",
            "  window command: w {shell}"
        ]
    );
}

// -------------------------------------------------------------- migrate

/// Stops every host and Sway operation from some point on, as if i3mux
/// died there. Counts the operations that ran.
#[derive(Default)]
struct Cut {
    /// How many more operations may run; `None`: all of them.
    left: Mutex<Option<usize>>,
    ran: Mutex<usize>,
}

impl Cut {
    fn at(&self, operations: Option<usize>) {
        *self.left.lock().unwrap() = operations;
    }

    fn ran(&self) -> usize {
        *self.ran.lock().unwrap()
    }

    /// Whether the next operation runs.
    fn pass(&self) -> bool {
        let mut left = self.left.lock().unwrap();
        match *left {
            Some(0) => return false,
            Some(n) => *left = Some(n - 1),
            None => {}
        }
        *self.ran.lock().unwrap() += 1;
        true
    }
}

/// A [`FakeHost`] behind a [`Cut`].
struct CutHost {
    inner: FakeHost,
    cut: Arc<Cut>,
}

impl CutHost {
    fn pass(&self) -> Result<(), HostError> {
        if self.cut.pass() {
            return Ok(());
        }
        Err(HostError::Unreachable {
            host: RemoteHost::new("cut").unwrap(),
            detail: "i3mux was cut here".into(),
        })
    }
}

impl HostExec for CutHost {
    fn host(&self) -> &Host {
        self.inner.host()
    }
    fn run(&self, argv: &[String], stdin: Option<&str>) -> Result<String, HostError> {
        self.pass()?;
        self.inner.run(argv, stdin)
    }
    fn attach_argv(&self, argv: &[String]) -> Vec<String> {
        self.inner.attach_argv(argv)
    }
    fn write_file_atomic(&self, path: &str, content: &str) -> Result<(), HostError> {
        self.pass()?;
        self.inner.write_file_atomic(path, content)
    }
    fn read_file(&self, path: &str) -> Result<Option<String>, HostError> {
        self.pass()?;
        self.inner.read_file(path)
    }
    fn list_dir(&self, dir: &str) -> Result<Vec<String>, HostError> {
        self.pass()?;
        self.inner.list_dir(dir)
    }
    fn remove_file(&self, path: &str) -> Result<bool, HostError> {
        self.pass()?;
        self.inner.remove_file(path)
    }
}

/// The fake Sway behind a [`Cut`].
struct CutSway {
    inner: Arc<FakeSway>,
    cut: Arc<Cut>,
}

impl Sway for CutSway {
    fn get_tree(&self) -> Result<Value> {
        anyhow::ensure!(self.cut.pass(), "i3mux was cut here");
        self.inner.get_tree()
    }
    fn run(&self, command: &str) -> Result<()> {
        anyhow::ensure!(self.cut.pass(), "i3mux was cut here");
        self.inner.run(command)
    }
}

/// A world for moves. Session ws4 is bound to workspace 4 on this machine
/// (the old host). Its shells 001 and 002 have windows there, next to
/// Firefox; 003 is an orphan; its layout is saved; session ws5 has a shell
/// and a layout too. The new host, deepthought, already has ws4-002 (made
/// by another program) and ws9-001. Every host and Sway operation passes
/// `cut`.
struct Moving {
    world: World,
    new: FakeHost,
    cut: Arc<Cut>,
    sway: CutSway,
    hosts: Box<HostFactory>,
}

impl Moving {
    fn new() -> Self {
        let world = World::new(Host::Local);
        world.bind("4", "ws4");
        for (shell, attached) in [
            ("ws4-001", true),
            ("ws4-002", true),
            ("ws4-003", false),
            ("ws5-001", false),
        ] {
            world.host.add_shell(shell, attached);
        }
        world.sway.open_marked("4", &world.mark("ws4-001"));
        world.sway.open_marked("4", &world.mark("ws4-002"));
        world.sway.open("4", "firefox");
        world.save_layout("ws4", &[1, 2]);
        world.save_layout("ws5", &[1]);
        let new = FakeHost::new(deepthought(), world.masters.clone());
        new.add_shell("ws4-002", false);
        new.add_shell("ws9-001", false);
        let cut = Arc::new(Cut::default());
        let sway = CutSway {
            inner: Arc::clone(&world.sway),
            cut: Arc::clone(&cut),
        };
        let (old, new_host, factory_cut) = (world.host.clone(), new.clone(), Arc::clone(&cut));
        let hosts: Box<HostFactory> = Box::new(move |h: &Host| -> Box<dyn HostExec> {
            let inner = if h == old.host() {
                old.clone()
            } else {
                assert_eq!(h, new_host.host(), "unexpected host {h}");
                new_host.clone()
            };
            Box::new(CutHost {
                inner,
                cut: Arc::clone(&factory_cut),
            })
        });
        Self {
            world,
            new,
            cut,
            sway,
            hosts,
        }
    }

    fn ports(&self) -> Ports<'_> {
        Ports {
            sway: &self.sway,
            hosts: self.hosts.as_ref(),
            ..self.world.ports()
        }
    }

    /// `attach --migrate` of ws4 to `to`, and the previews it announced.
    fn migrate(&self, to: &Host) -> (Result<Migration>, Vec<Preview>) {
        let mut previews = Vec::new();
        let moved = migrate(&self.ports(), to, &name("ws4"), &mut |p: &Preview| {
            previews.push(p.clone());
        });
        (moved, previews)
    }

    /// What a move may change, to compare runs.
    fn end_state(&self) -> EndState {
        let state = self.world.store.load().unwrap();
        let tree = self.world.sway.get_tree().unwrap();
        let mut marks: Vec<String> = window::i3mux_windows_in(&tree)
            .into_iter()
            .flat_map(|(id, _)| window::marks_of(&tree, id))
            .map(|mark| mark.to_string())
            .collect();
        marks.sort();
        let files = |host: &FakeHost| host.state.lock().unwrap().files.clone();
        let shells = |host: &FakeHost| host.shells().into_keys().collect();
        EndState {
            binding: state.get("4").map(|b| {
                let cleanup = b.cleanup().map(|c| c.host().clone());
                (b.host().clone(), cleanup)
            }),
            marks,
            windows: self.world.sway.window_count(),
            old_shells: shells(&self.world.host),
            new_shells: shells(&self.new),
            old_files: files(&self.world.host),
            new_files: files(&self.new),
        }
    }

    fn created_on_new(&self) -> Vec<(String, Option<String>)> {
        self.new.state.lock().unwrap().created.clone()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct EndState {
    /// The host of ws4's binding, and of its pending cleanup.
    binding: Option<(Host, Option<Host>)>,
    /// Every i3mux mark on every window, sorted.
    marks: Vec<String>,
    windows: usize,
    old_shells: Vec<String>,
    new_shells: Vec<String>,
    old_files: BTreeMap<String, String>,
    new_files: BTreeMap<String, String>,
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// Where a whole move of [`Moving`] ends: ws4 is on deepthought with no
/// pending cleanup, its windows say so, and none of its shells or its
/// layout is left on this machine (rules 2 and 3).
fn moved_end() -> EndState {
    let mut old_files = Moving::new().world.host.state.lock().unwrap().files.clone();
    let layout = old_files.remove(SAVED_WS4).unwrap();
    EndState {
        binding: Some((deepthought(), None)),
        marks: strings(&["_i3mux:deepthought:ws4-001", "_i3mux:deepthought:ws4-002"]),
        windows: 3,
        old_shells: strings(&["ws5-001"]),
        new_shells: strings(&["ws4-001", "ws4-002", "ws4-003", "ws9-001"]),
        old_files,
        new_files: BTreeMap::from([(SAVED_WS4.to_owned(), layout)]),
    }
}

/// PLAN-migrate decisions 3 to 6: the move says what will happen first,
/// keeps the shell that the new host has, makes the missing ones there as
/// login shells (no command), moves the binding, re-marks the windows,
/// copies the layout, and ends the session on the old host. No window
/// opens or closes, and other sessions are left alone.
#[test]
fn migrate_moves_a_session_and_its_windows_to_the_new_host() {
    let moving = Moving::new();

    let (moved, previews) = moving.migrate(&deepthought());

    assert_eq!(
        moved.unwrap(),
        Migration::Moved {
            from: Host::Local,
            done: MoveDone {
                shells: 3,
                started: 2,
                remarked: 2,
                ended: 3
            }
        }
    );
    assert_eq!(
        previews.iter().map(format_preview).collect::<Vec<_>>(),
        [
            "moving ws4 from local to deepthought: 2 of 3 shells start again there as login \
          shells in ~; programs running on local end."
        ]
    );
    assert_eq!(moving.end_state(), moved_end());
    assert_eq!(
        moving.created_on_new(),
        [("ws4-001".to_owned(), None), ("ws4-003".to_owned(), None)],
        "ws4-002 is kept"
    );
    assert!(moving.world.launches().is_empty());
}

/// The preview's wording for one shell, and for a rerun.
#[test]
fn a_move_says_what_will_happen() {
    let one = Preview::Move {
        session: name("ws3"),
        from: Host::Ssh(RemoteHost::new("s0nny-box").unwrap()),
        to: Host::Ssh(RemoteHost::new("dev-box").unwrap()),
        missing: 1,
        shells: 2,
    };
    assert_eq!(
        format_preview(&one),
        "moving ws3 from s0nny-box to dev-box: 1 of 2 shells starts again there as a login \
         shell in ~; programs running on s0nny-box end."
    );
    let finish = Preview::Finish {
        session: name("ws3"),
        from: Host::Local,
        to: deepthought(),
    };
    assert_eq!(
        format_preview(&finish),
        "finishing the move of ws3 from local to deepthought: programs running on local end."
    );
}

/// PLAN-migrate rules 1 to 3 (Enforcement): cut the move after each host
/// and Sway operation, then run it again. Every rerun reaches the end of
/// a whole move. Before the binding moves, only shells on the new host
/// have changed; once it moved, every shell of the session on the old host
/// exists on the new one.
#[test]
fn a_move_cut_at_any_step_then_run_again_reaches_the_same_end() {
    let whole = Moving::new();
    whole.migrate(&deepthought()).0.unwrap();
    let operations = whole.cut.ran();
    assert!(operations > 10, "{operations}");
    let start = Moving::new().end_state();

    for cut_at in 0..operations {
        let moving = Moving::new();
        moving.cut.at(Some(cut_at));
        let (cut_run, _) = moving.migrate(&deepthought());
        assert!(cut_run.is_err(), "cut at {cut_at} of {operations}");
        moving.cut.at(None);
        let cut = moving.end_state();
        match &cut.binding {
            Some((host, None)) if *host == Host::Local => {
                assert_eq!(
                    EndState {
                        new_shells: start.new_shells.clone(),
                        ..cut
                    },
                    start,
                    "rule 1, cut at {cut_at}"
                );
            }
            Some((host, Some(left))) if *host == deepthought() && *left == Host::Local => {
                for shell in ["ws4-001", "ws4-002", "ws4-003"] {
                    assert!(
                        cut.new_shells.iter().any(|s| s == shell),
                        "rule 2, cut at {cut_at}: {shell}"
                    );
                }
            }
            other => panic!("cut at {cut_at}: binding {other:?}"),
        }

        let (rerun, _) = moving.migrate(&deepthought());

        rerun.unwrap();
        assert_eq!(moving.end_state(), moved_end(), "cut at {cut_at}");
    }
}

/// PLAN-migrate decision 7: once the move is done, another one prints
/// that the session is there and changes nothing.
#[test]
fn a_move_to_where_the_session_is_changes_nothing() {
    let moving = Moving::new();
    moving.migrate(&deepthought()).0.unwrap();
    let commands = moving.world.sway.commands().len();

    let (again, previews) = moving.migrate(&deepthought());

    assert_eq!(again.unwrap(), Migration::AlreadyThere);
    assert!(previews.is_empty());
    assert_eq!(moving.end_state(), moved_end());
    assert_eq!(moving.world.sway.commands().len(), commands);
}

/// PLAN-migrate decision 7: a rerun of a move whose cleanup is pending
/// re-marks the windows left with the old host and runs the cleanup.
#[test]
fn a_rerun_finishes_a_move_with_a_pending_cleanup() {
    let moving = Moving::new();
    moving.new.add_shell("ws4-001", false);
    moving.new.add_shell("ws4-003", false);
    moving
        .world
        .store
        .with_lock(|s| Ok(s.migrate("4", &deepthought())?))
        .unwrap();

    let (finished, previews) = moving.migrate(&deepthought());

    assert_eq!(
        finished.unwrap(),
        Migration::Finished {
            from: Host::Local,
            done: MoveDone {
                remarked: 2,
                ended: 3,
                ..MoveDone::default()
            }
        }
    );
    assert!(matches!(previews.as_slice(), [Preview::Finish { .. }]));
    assert_eq!(moving.end_state(), moved_end());
    assert!(moving.created_on_new().is_empty());
}

/// PLAN-migrate decision 2: a move to the host of a pending cleanup cancels
/// that cleanup and keeps that host's shells; the new cleanup names the
/// host the session leaves, and runs.
#[test]
fn a_move_back_cancels_the_cleanup_and_keeps_that_hosts_shells() {
    let moving = Moving::new();
    for shell in ["ws4-001", "ws4-003", "ws4-004"] {
        moving.new.add_shell(shell, false);
    }
    moving
        .world
        .store
        .with_lock(|s| Ok(s.migrate("4", &deepthought())?))
        .unwrap();
    let (old_shells, old_files) = (
        moving.world.host.shells(),
        moving.world.host.state.lock().unwrap().files.clone(),
    );

    let (back, _) = moving.migrate(&Host::Local);

    assert_eq!(
        back.unwrap(),
        Migration::Moved {
            from: deepthought(),
            done: MoveDone {
                shells: 4,
                started: 1,
                remarked: 0,
                ended: 4
            }
        }
    );
    let end = moving.end_state();
    assert_eq!(end.binding, Some((Host::Local, None)));
    assert_eq!(end.new_shells, ["ws9-001"]);
    let created = moving.world.host.state.lock().unwrap().created.clone();
    assert_eq!(
        created,
        [("ws4-004".to_owned(), None)],
        "001 to 003 are kept"
    );
    let mut expected = old_shells;
    expected.insert("ws4-004".to_owned(), false);
    assert_eq!(moving.world.host.shells(), expected);
    assert_eq!(
        moving.world.host.state.lock().unwrap().files,
        old_files,
        "this machine keeps its layout; deepthought had none to copy"
    );
    assert_eq!(
        end.marks,
        ["_i3mux:local:ws4-001", "_i3mux:local:ws4-002"],
        "the windows never left"
    );
}

/// Run `migrate` on `moving` and check that it is refused with nothing
/// changed: no preview, no shell made, the same end state.
fn refused(moving: &Moving, to: &Host) -> anyhow::Error {
    let before = moving.end_state();
    let (moved, previews) = moving.migrate(to);
    let err = moved.unwrap_err();
    assert!(previews.is_empty(), "{err:#}");
    assert!(moving.created_on_new().is_empty(), "{err:#}");
    assert_eq!(moving.end_state(), before, "{err:#}");
    err
}

/// PLAN-migrate decision 2: a new host with a saved layout of the session,
/// or a binding of it in another workspace, would be overwritten.
#[test]
fn a_move_that_would_overwrite_another_session_is_refused() {
    let saved = Moving::new();
    let layout = saved.world.host.file(SAVED_WS4).unwrap();
    saved
        .new
        .state
        .lock()
        .unwrap()
        .files
        .insert(SAVED_WS4.to_owned(), layout);
    let err = refused(&saved, &deepthought());
    assert!(
        matches!(command_error(&err), CommandError::MoveTargetSaved { .. }),
        "{err:#}"
    );
    assert!(
        err.to_string()
            .contains("run 'i3mux kill --remote deepthought --session ws4'"),
        "{err}"
    );

    let bound = Moving::new();
    let other = WorkspaceState::new(deepthought(), name("ws4"));
    bound
        .world
        .store
        .with_lock(|s| Ok(s.bind("5", other)?))
        .unwrap();
    let err = refused(&bound, &deepthought());
    assert!(
        matches!(command_error(&err), CommandError::MoveTargetBound { workspace, .. } if workspace == "5"),
        "{err:#}"
    );
}

/// PLAN-migrate decision 2: a pending cleanup of a third host would be
/// lost. The refusal names the command that finishes it.
#[test]
fn a_move_with_a_cleanup_of_a_third_host_is_refused() {
    let moving = Moving::new();
    moving
        .world
        .store
        .with_lock(|s| Ok(s.migrate("4", &deepthought())?))
        .unwrap();
    let third = Host::Ssh(RemoteHost::new("third").unwrap());

    let err = refused(&moving, &third);

    assert!(
        matches!(command_error(&err), CommandError::MoveUnfinished { .. }),
        "{err:#}"
    );
    assert!(
        err.to_string()
            .contains("'i3mux attach --remote deepthought --session ws4 --migrate'"),
        "{err}"
    );
}

/// PLAN-migrate decision 2: either host not answering refuses the move.
#[test]
fn a_move_with_a_host_that_does_not_answer_is_refused() {
    for silent in [Host::Local, deepthought()] {
        let moving = Moving::new();
        let host = if silent == Host::Local {
            &moving.world.host
        } else {
            &moving.new
        };
        host.state.lock().unwrap().unreachable = true;
        let before = moving.end_state();
        let (moved, previews) = moving.migrate(&deepthought());
        host.state.lock().unwrap().unreachable = false;
        let err = moved.unwrap_err();
        assert!(previews.is_empty(), "{err:#}");
        assert!(
            format!("{err:#}").contains("did not move, and nothing changed"),
            "{err:#}"
        );
        assert_eq!(moving.end_state(), before, "{err:#}");
    }
}

#[test]
fn a_move_of_a_session_bound_nowhere_or_twice_is_refused() {
    let world = World::new(Host::Local);
    let mut ignore = |_: &Preview| {};
    let err = migrate(&world.ports(), &deepthought(), &name("ws4"), &mut ignore).unwrap_err();
    assert!(matches!(
        command_error(&err),
        CommandError::NothingToMove(_)
    ));
    // Its own exit status: no binding (README, "Exit status").
    assert_eq!(failure_status(&err), EXIT_NO_BINDING);

    world.bind("4", "ws4");
    let third = WorkspaceState::new(Host::Ssh(RemoteHost::new("third").unwrap()), name("ws4"));
    world.store.with_lock(|s| Ok(s.bind("5", third)?)).unwrap();
    let err = migrate(&world.ports(), &deepthought(), &name("ws4"), &mut ignore).unwrap_err();
    assert!(
        err.to_string()
            .starts_with("Session 'ws4' is bound in workspaces 4 and 5"),
        "{err}"
    );
    assert_eq!(failure_status(&err), 1, "any other refusal");
}

/// PLAN-migrate decision 4 and rule 1: a shell that does not start on the
/// new host ends the shells this move made there, and nothing else
/// changes. The shell that the new host had stays.
#[test]
fn a_shell_that_does_not_start_ends_the_ones_made_and_changes_nothing_else() {
    let moving = Moving::new();
    moving.new.state.lock().unwrap().vanishing = vec!["ws4-003".to_owned()];
    let before = moving.end_state();

    let err = moving.migrate(&deepthought()).0.unwrap_err();

    let text = format!("{err:#}");
    assert!(
        text.contains("Shell ws4-003 did not start on deepthought"),
        "{text}"
    );
    // ws4-001 and ws4-003, whose `attach -b` worked before it ended.
    assert!(text.contains("ended the 2 shells it made there"), "{text}");
    assert_eq!(moving.end_state(), before);

    let failing = Moving::new();
    failing.new.state.lock().unwrap().fail_create = Some("shpool: no".into());
    let before = failing.end_state();
    let err = failing.migrate(&deepthought()).0.unwrap_err();
    assert!(
        format!("{err:#}").contains("cannot start shell ws4-001"),
        "{err:#}"
    );
    assert_eq!(failing.end_state(), before);
}

/// A move whose binding has been moved by `LocalState::migrate` and whose
/// windows show the new host: the state after a cut right after step 5.
fn moved_with_pending_cleanup() -> Moving {
    let moving = Moving::new();
    moving.new.add_shell("ws4-001", true);
    moving.new.add_shell("ws4-003", false);
    moving
        .world
        .store
        .with_lock(|s| Ok(s.migrate("4", &deepthought())?))
        .unwrap();
    let request = Move {
        workspace: "4",
        session: &name("ws4"),
        from: &Host::Local,
        to: &deepthought(),
    };
    remark_session(&moving.ports(), &request).unwrap();
    moving
}

/// PLAN-migrate decision 8: `detach` and `kill` run a pending cleanup
/// first: the old host's shells and layout of the session end.
#[test]
fn detach_and_kill_run_a_pending_cleanup_first() {
    let detaching = moved_with_pending_cleanup();
    detach(&detaching.ports(), "4", 0).unwrap();
    let end = detaching.end_state();
    assert_eq!(end.binding, None);
    assert_eq!(end.old_shells, ["ws5-001"]);
    assert!(!end.old_files.contains_key(SAVED_WS4));
    assert!(
        end.new_files.contains_key(SAVED_WS4),
        "detach saved it there"
    );

    let killing = moved_with_pending_cleanup();
    kill(&killing.ports(), &deepthought(), &name("ws4")).unwrap();
    let end = killing.end_state();
    assert_eq!(end.binding, None);
    assert_eq!(end.old_shells, ["ws5-001"]);
    assert_eq!(end.new_shells, ["ws9-001"]);
    assert!(!end.old_files.contains_key(SAVED_WS4));
    assert!(!end.new_files.contains_key(SAVED_WS4));
}

/// PLAN-migrate decision 8: with the old host silent, `detach` and `kill`
/// refuse, and the binding keeps its pending cleanup.
#[test]
fn detach_and_kill_refuse_while_the_old_host_of_a_cleanup_does_not_answer() {
    for command in ["detach", "kill"] {
        let moving = moved_with_pending_cleanup();
        moving.world.host.state.lock().unwrap().unreachable = true;
        let before = moving.end_state();

        let err = match command {
            "detach" => detach(&moving.ports(), "4", 0).map(drop),
            _ => kill(&moving.ports(), &deepthought(), &name("ws4")).map(drop),
        }
        .unwrap_err();

        assert!(
            matches!(command_error(&err), CommandError::CleanupUnanswered { .. }),
            "{command}: {err:#}"
        );
        assert_eq!(moving.end_state(), before, "{command}");
    }
}

/// PLAN-migrate decision 8: `sessions` and `status` show a pending cleanup
/// with the command that finishes it.
#[test]
fn sessions_and_status_show_a_pending_cleanup_and_how_to_finish_it() {
    let moving = moved_with_pending_cleanup();
    let finish = "shells on local not ended yet: \
                  run 'i3mux attach --remote deepthought --session ws4 --migrate'";

    let rows = sessions(&moving.ports(), &deepthought()).unwrap();
    let report = status(&moving.ports(), "4").unwrap().unwrap();

    assert_eq!(
        format_rows(&rows)[0],
        format!("ws4  (deepthought)  workspace 4, 2 windows, 1 orphan, {finish}")
    );
    assert_eq!(
        format_status(&report)[0],
        format!("ws4  (deepthought)  workspace 4, 2 windows, {finish}")
    );
}

// ------------------------------------------------------------- reattach

/// PLAN-migrate decision 9: a window whose session is bound to another
/// host that has its shell follows it. The argv is the wrapper's attach of
/// the same shell there, with the window's options and without `--force`;
/// the new host's master starts first. The window's own host is never
/// asked (the world's host factory knows only the new host).
#[test]
fn reattach_follows_a_session_bound_to_another_host_that_has_the_shell() {
    let world = World::new(deepthought());
    world.bind("4", "ws4");
    world.host.add_shell("ws4-002", false);
    let options = WrapperOptions {
        no_hold: true,
        attach_only: true,
    };

    let Reattach::Follow(follow) =
        reattach(&world.ports(), &t("ws4", 2), &Host::Local, options).unwrap()
    else {
        panic!("no follow")
    };

    assert_eq!(follow.mark, Mark::new(deepthought(), t("ws4", 2)));
    assert_eq!(
        follow.argv[..2],
        [format!("{options:?}"), follow.mark.title()]
    );
    assert_eq!(follow.argv[follow.argv.len() - 2..], ["attach", "ws4-002"]);
    assert_eq!(
        world.fakes.ensured(),
        [dt(), dt()],
        "the list, then the window's master"
    );
    assert!(world.launches().is_empty(), "it opens no window");
    assert!(world.sway.commands().is_empty(), "it marks nothing");
}

/// A new host that does not answer, or whose master does not start, makes
/// the window wait: it must not attach on its old host meanwhile.
#[test]
fn reattach_waits_while_the_new_host_does_not_answer() {
    let world = World::new(deepthought());
    world.bind("4", "ws4");
    world.host.add_shell("ws4-002", false);
    world.host.state.lock().unwrap().unreachable = true;
    let wait = reattach(
        &world.ports(),
        &t("ws4", 2),
        &Host::Local,
        WrapperOptions::default(),
    );
    assert!(
        matches!(wait, Ok(Reattach::Wait { ref host, .. }) if *host == deepthought()),
        "{wait:?}"
    );

    world.host.state.lock().unwrap().unreachable = false;
    world.fakes.log.lock().unwrap().fail_start = true;
    let wait = reattach(
        &world.ports(),
        &t("ws4", 2),
        &Host::Local,
        WrapperOptions::default(),
    );
    assert!(matches!(wait, Ok(Reattach::Wait { .. })), "{wait:?}");
    let outcome = cmd_reattach(
        &world.ports(),
        &t("ws4", 2),
        &Host::Local,
        WrapperOptions::default(),
    )
    .unwrap();
    assert!(
        matches!(outcome, Outcome::Exit(code) if code == ExitCode::from(EXIT_FOLLOW_WAIT)),
        "{outcome:?}"
    );
}

/// The window stays when its session did not move, is bound nowhere or on
/// two other hosts, or moved to a host that lacks the shell. A session
/// that did not move contacts no host.
#[test]
fn reattach_stays_unless_the_session_moved_to_a_host_with_the_shell() {
    let stays = |world: &World, from: &Host| {
        let outcome = reattach(
            &world.ports(),
            &t("ws4", 2),
            from,
            WrapperOptions::default(),
        );
        matches!(outcome, Ok(Reattach::Stay))
    };
    let world = World::new(deepthought());
    world.host.add_shell("ws4-002", false);
    assert!(stays(&world, &Host::Local), "bound nowhere");
    world.bind("4", "ws4");
    assert!(stays(&world, &deepthought()), "not moved");
    assert!(
        world.fakes.events().is_empty(),
        "{:?}",
        world.fakes.events()
    );
    world.host.state.lock().unwrap().shells.clear();
    assert!(stays(&world, &Host::Local), "the new host lacks the shell");
    let elsewhere = WorkspaceState::new(Host::Ssh(RemoteHost::new("other").unwrap()), name("ws4"));
    world
        .store
        .with_lock(|s| Ok(s.bind("5", elsewhere)?))
        .unwrap();
    assert!(stays(&world, &Host::Local), "bound on two other hosts");
    let outcome = cmd_reattach(
        &world.ports(),
        &t("ws4", 2),
        &Host::Local,
        WrapperOptions::default(),
    )
    .unwrap();
    assert!(
        matches!(outcome, Outcome::Exit(code) if code == ExitCode::from(EXIT_STAY)),
        "{outcome:?}"
    );
}

/// The wrapper reads the argv as NUL-terminated words; a word may hold
/// newlines, as the shpool prelude does.
#[test]
fn a_follow_prints_each_word_nul_terminated() {
    let words = ["sh".to_owned(), "a\nb".to_owned(), String::new()];
    assert_eq!(nul_terminated(&words), b"sh\0a\nb\0\0");
}

// ------------------------------------------------------- terminal --here

/// The terminal `here` runs in: a window titled "nonce" on `workspace`,
/// next to another terminal that has focus, as when the user switched
/// windows while `here` started.
fn here_terminal(world: &World, workspace: &str) -> u64 {
    let id = world.sway.open_titled(workspace, "foot", "nonce");
    world.sway.open_titled(workspace, "foot", "other");
    id
}

/// The title and the argv of a `HereExec`, as `FakeLauncher::program_argv`
/// makes the argv from `options`, `title` and `command`.
fn here_exec(options: WrapperOptions, title: &str, command: &[String]) -> (String, Vec<String>) {
    let mut argv = vec![format!("{options:?}"), title.to_owned()];
    argv.extend_from_slice(command);
    (title.to_owned(), argv)
}

/// PLAN-terminal-here decision 1: in an unbound workspace, `here` binds it
/// to this machine and its default session, marks the terminal it runs in
/// (found by its id, not focus), and runs the wrapper's attach of a new
/// shell there. It opens no window.
#[test]
fn here_binds_an_unbound_workspace_and_marks_its_own_window() {
    let world = World::new(Host::Local);
    let id = here_terminal(&world, "4");
    let request = HereRequest {
        terminal: TerminalRequest {
            no_hold: true,
            ..TerminalRequest::default()
        },
        ..HereRequest::default()
    };

    let exec = world.finish(|ports| here(ports, id, &request)).unwrap();

    let binding = world.store.load().unwrap().get("4").cloned().unwrap();
    assert_eq!(binding, WorkspaceState::new(Host::Local, name("ws4")));
    assert_eq!(
        world.sway.commands(),
        [window::mark_command(id, &world.mark("ws4-001"))]
    );
    let attach = mux::attach_argv(&world.host, &t("ws4", 1), false, None);
    let options = WrapperOptions {
        no_hold: true,
        attach_only: false,
    };
    assert_eq!(
        (exec.title, exec.argv),
        here_exec(options, "i3mux:local:ws4-001", &attach)
    );
    assert!(world.launches().is_empty(), "no window opens");
}

/// `--remote` binds an unbound workspace to that host. The window goes
/// through the host's master, which stays after the command because the
/// binding names the host.
#[test]
fn here_with_remote_binds_an_unbound_workspace_to_that_host() {
    let world = World::new(deepthought());
    let id = here_terminal(&world, "4");
    let request = HereRequest {
        host: Some(deepthought()),
        ..HereRequest::default()
    };

    world.finish(|ports| here(ports, id, &request)).unwrap();

    let binding = world.store.load().unwrap().get("4").cloned().unwrap();
    assert_eq!(binding.host(), &deepthought());
    assert!(world.fakes.running(&dt()), "{:?}", world.fakes.events());
    assert_eq!(
        world.sway.commands(),
        [window::mark_command(id, &world.mark("ws4-001"))]
    );
}

/// A workspace bound to another host than `--remote`, or with another
/// window command, is refused before anything changes: no host is asked
/// (the world's host factory knows only the bound host), nothing is marked
/// and the binding stays.
#[test]
fn here_refuses_a_workspace_bound_otherwise_before_changing_anything() {
    let world = World::new(Host::Local);
    world.bind_with("4", "ws4", "w {shell}");
    let id = here_terminal(&world, "4");
    let before = world.store.load().unwrap();
    let other_host = HereRequest {
        host: Some(deepthought()),
        ..HereRequest::default()
    };
    let other_command = HereRequest {
        window_command: Some(template("v {shell}")),
        ..HereRequest::default()
    };

    let err = here(&world.ports(), id, &other_host).unwrap_err();
    assert!(
        matches!(command_error(&err), CommandError::BoundToOtherHost { .. }),
        "{err}"
    );
    assert_eq!(
        err.to_string(),
        "Workspace 4 is bound to session 'ws4' on local, not deepthought; nothing changed"
    );
    let err = here(&world.ports(), id, &other_command).unwrap_err();
    assert!(
        matches!(command_error(&err), CommandError::OtherWindowCommand(_)),
        "{err}"
    );

    assert!(world.sway.commands().is_empty());
    assert_eq!(world.store.load().unwrap(), before);
    assert!(world.created().is_empty());
}

/// The same `--remote` and window command as the binding's are no
/// conflict: `here` joins the bound session.
#[test]
fn here_joins_a_workspace_bound_as_asked() {
    let world = World::new(Host::Local);
    world.bind_with("4", "ws4", "w {shell}");
    world.host.add_shell("ws4-001", true);
    let id = here_terminal(&world, "4");
    let request = HereRequest {
        host: Some(Host::Local),
        window_command: Some(template("w {shell}")),
        ..HereRequest::default()
    };

    let exec = here(&world.ports(), id, &request).unwrap();

    assert_eq!(exec.title, "i3mux:local:ws4-002");
}

/// PLAN-terminal-here decision 2: without `--shell`, `here` runs the
/// binding's window command, after it made the new shell (with `-e`'s
/// command), so the window command can attach it.
#[test]
fn here_runs_the_window_command_and_makes_a_new_shell_first() {
    let world = World::new(Host::Local);
    world.bind_with("4", "ws4", "w {session} {shell}");
    world.host.add_shell("ws4-001", false);
    let id = here_terminal(&world, "4");
    let request = HereRequest {
        terminal: TerminalRequest {
            command: Some("htop".into()),
            ..TerminalRequest::default()
        },
        ..HereRequest::default()
    };

    let exec = here(&world.ports(), id, &request).unwrap();

    assert_eq!(
        world.created(),
        [("ws4-002".to_owned(), Some("htop".into()))]
    );
    let window = ["w".to_owned(), "ws4".into(), "ws4-002".into()];
    assert_eq!(
        (exec.title, exec.argv),
        here_exec(WrapperOptions::default(), "i3mux:local:ws4-002", &window)
    );
}

/// A window command that attaches its shell (`i3mux terminal --here
/// --shell {shell}`) finds it free: shpool lists a shell that `attach -b`
/// made as attached for a moment, and i3mux returns from making it only
/// once no client shows it. Without the wait, `--here --shell` in the
/// window refuses the shell as attached elsewhere.
#[test]
fn a_window_command_can_attach_the_shell_i3mux_made_for_it() {
    let world = World::new(Host::Local);
    world.host.state.lock().unwrap().create_lag = 3;
    world.bind_with("4", "ws4", "i3mux terminal --here --shell {shell}");
    let first = world.sway.open_titled("4", "foot", "first");

    here(&world.ports(), first, &HereRequest::default()).unwrap();
    // The window command's own `--here --shell`, in the window i3mux
    // just marked.
    let attach = HereRequest {
        shell: Some(t("ws4", 1)),
        ..HereRequest::default()
    };

    let exec = here(&world.ports(), first, &attach).unwrap();

    assert_eq!(world.created(), [("ws4-001".to_owned(), None)]);
    assert_eq!(exec.title, "i3mux:local:ws4-001");
}

/// `--shell` attaches that shell through the wrapper, which never starts a
/// new one (`attach_only`); the window command is not run and no shell is
/// made.
#[test]
fn here_shell_attaches_that_shell_without_the_window_command() {
    let world = World::new(Host::Local);
    world.bind_with("4", "ws4", "w {shell}");
    world.host.add_shell("ws4-003", false);
    let id = here_terminal(&world, "4");
    let request = HereRequest {
        shell: Some(t("ws4", 3)),
        ..HereRequest::default()
    };

    let exec = here(&world.ports(), id, &request).unwrap();

    assert!(world.created().is_empty());
    let attach = mux::attach_argv(&world.host, &t("ws4", 3), false, None);
    let options = WrapperOptions {
        no_hold: false,
        attach_only: true,
    };
    assert_eq!(
        (exec.title, exec.argv),
        here_exec(options, "i3mux:local:ws4-003", &attach)
    );
    assert_eq!(
        world.sway.commands(),
        [window::mark_command(id, &world.mark("ws4-003"))]
    );
}

/// `--shell` never makes a shell (PLAN-terminal-here finding 3). It
/// refuses a missing shell, a shell of another session, and a shell
/// attached elsewhere unless `--force`, before anything changes.
/// A window that kg moved to another workspace keeps its shell: `here
/// --shell` takes the binding of the shell's own session, wherever the
/// window is now, attaches on that binding's host, and changes no binding.
#[test]
fn here_shell_follows_its_own_session_from_another_workspace() {
    let world = World::new(Host::Local);
    world.bind("3", "ws3");
    world.host.add_shell("ws3-001", false);
    let id = here_terminal(&world, "5");
    let request = HereRequest {
        shell: Some(t("ws3", 1)),
        terminal: TerminalRequest {
            no_hold: true,
            ..TerminalRequest::default()
        },
        ..HereRequest::default()
    };

    let exec = here(&world.ports(), id, &request).unwrap();

    assert_eq!(exec.title, "i3mux:local:ws3-001");
    let state = world.store.load().unwrap();
    assert!(state.get("5").is_none(), "workspace 5 was bound");
    assert_eq!(
        state.get("3").map(|b| b.session().to_string()).as_deref(),
        Some("ws3")
    );
}

#[test]
fn here_shell_refuses_a_missing_foreign_or_attached_shell() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", true);
    world.host.add_shell("ws5-001", false);
    let id = here_terminal(&world, "4");
    let shell = |terminal: TerminalName, force: bool| HereRequest {
        shell: Some(terminal),
        terminal: TerminalRequest {
            force,
            ..TerminalRequest::default()
        },
        ..HereRequest::default()
    };

    let missing = here(&world.ports(), id, &shell(t("ws4", 2), false)).unwrap_err();
    let foreign = here(&world.ports(), id, &shell(t("ws5", 1), false)).unwrap_err();
    let attached = here(&world.ports(), id, &shell(t("ws4", 1), false)).unwrap_err();

    assert!(
        matches!(command_error(&missing), CommandError::NoSuchShell { .. }),
        "{missing}"
    );
    // Its own exit status: no shell (README, "Exit status").
    assert_eq!(failure_status(&missing), EXIT_NO_SHELL);
    assert_eq!(failure_status(&foreign), 1);
    assert!(
        foreign
            .to_string()
            .ends_with("; move this window to the workspace of session 'ws5'"),
        "a window moved to another workspace is told where it belongs: {foreign}"
    );
    assert!(
        matches!(
            command_error(&foreign),
            CommandError::ShellOfOtherSession { .. }
        ),
        "{foreign}"
    );
    assert!(
        matches!(command_error(&attached), CommandError::AlreadyAttached(_)),
        "{attached}"
    );
    assert!(world.sway.commands().is_empty());
    assert!(world.created().is_empty());

    let exec = here(&world.ports(), id, &shell(t("ws4", 1), true)).unwrap();
    assert_eq!(exec.title, "i3mux:local:ws4-001");
}

/// The window keeps one i3mux mark: `here` in a window that is already
/// marked with its shell sends nothing, and in one marked with another
/// shell it replaces that mark.
#[test]
fn here_replaces_the_windows_other_i3mux_mark() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", false);
    world.host.add_shell("ws4-002", false);
    let id = world.sway.open_marked("4", &world.mark("ws4-001"));
    let shell = |n| HereRequest {
        shell: Some(t("ws4", n)),
        ..HereRequest::default()
    };

    here(&world.ports(), id, &shell(1)).unwrap();
    assert!(world.sway.commands().is_empty(), "already marked");

    here(&world.ports(), id, &shell(2)).unwrap();
    assert_eq!(
        world.sway.commands(),
        [
            window::unmark_command(id, &world.mark("ws4-001")),
            window::mark_command(id, &world.mark("ws4-002")),
        ]
    );
    assert_eq!(world.window_numbers("ws4"), [2]);
}

/// An unreachable host binds and marks nothing.
#[test]
fn here_on_an_unreachable_host_binds_and_marks_nothing() {
    let world = World::new(Host::Local);
    world.host.state.lock().unwrap().unreachable = true;
    let id = here_terminal(&world, "4");

    here(&world.ports(), id, &HereRequest::default()).unwrap_err();

    assert!(world.store.load().unwrap().get("4").is_none());
    assert!(world.sway.commands().is_empty());
}

/// The windows that carry `terminal`'s mark, by container id.
fn windows_of(world: &World, terminal: &str) -> Vec<u64> {
    let mark = world.mark(terminal);
    window::i3mux_windows_in(&world.sway.get_tree().unwrap())
        .into_iter()
        .filter(|(_, m)| *m == mark)
        .map(|(id, _)| id)
        .collect()
}

/// `--shell` never gives a shell a second window: a shell that another
/// window carries is refused before anything changes, even when no client
/// is attached (that window lost its client, or shows a landing page).
#[test]
fn here_shell_never_gives_a_shell_a_second_window() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", false);
    let a = world.sway.open_marked("4", &world.mark("ws4-001"));
    let b = world.sway.open_titled("4", "foot", "nonce");
    let request = HereRequest {
        shell: Some(t("ws4", 1)),
        ..HereRequest::default()
    };

    let err = here(&world.ports(), b, &request).unwrap_err();

    assert!(
        matches!(command_error(&err), CommandError::ShownElsewhere(_)),
        "{err}"
    );
    assert_eq!(windows_of(&world, "ws4-001"), [a]);
    assert!(world.sway.commands().is_empty());
}

/// With `--force`, the shell's mark moves: the other window loses it, so
/// the shell still has one window.
#[test]
fn here_shell_with_force_moves_the_mark_from_the_other_window() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", true);
    let a = world.sway.open_marked("4", &world.mark("ws4-001"));
    let b = world.sway.open_titled("4", "foot", "nonce");
    let request = HereRequest {
        shell: Some(t("ws4", 1)),
        terminal: TerminalRequest {
            force: true,
            ..TerminalRequest::default()
        },
        ..HereRequest::default()
    };

    here(&world.ports(), b, &request).unwrap();

    assert_eq!(windows_of(&world, "ws4-001"), [b]);
    assert!(marks_of_window(&world, a).is_empty());
}

/// The window that already carries the shell's mark is not "another
/// window": `--shell` in it is never refused and needs no `--force`, also
/// when the binding moved to another host since (`attach --migrate`
/// re-marked the window).
#[test]
fn here_shell_in_the_window_that_has_the_mark_needs_no_force() {
    let world = World::new(deepthought());
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", false);
    let id = world.sway.open_marked("4", &world.mark("ws4-001"));
    let request = HereRequest {
        host: Some(deepthought()),
        shell: Some(t("ws4", 1)),
        ..HereRequest::default()
    };

    let exec = here(&world.ports(), id, &request).unwrap();

    assert_eq!(exec.changes.marks, MarkChanges::default());
    assert!(world.sway.commands().is_empty());
}

/// Every i3mux mark on window `id`.
fn marks_of_window(world: &World, id: u64) -> Vec<Mark> {
    window::marks_of(&world.sway.get_tree().unwrap(), id)
}

/// A failed exec: `undo_here` takes back what `here` did in an unbound
/// workspace with a window command. The binding goes, the window loses its
/// mark, and the shell `here` made ends.
#[test]
fn undo_after_a_failed_exec_takes_back_the_binding_the_mark_and_the_shell() {
    let world = World::new(Host::Local);
    let id = here_terminal(&world, "4");
    let request = HereRequest {
        window_command: Some(template("no-such-program {shell}")),
        ..HereRequest::default()
    };
    let exec = here(&world.ports(), id, &request).unwrap();
    assert_eq!(world.created(), [("ws4-001".to_owned(), None)]);

    undo_here(&world.ports(), &exec.changes).unwrap();

    assert!(world.store.load().unwrap().get("4").is_none());
    assert!(marks_of_window(&world, id).is_empty());
    assert!(world.host.shells().is_empty(), "{:?}", world.host.shells());
}

/// In a bound workspace, the undo keeps the binding and the shells it did
/// not make, and gives every window back the marks it had: the old mark of
/// this window, and the mark `--force` took from the other window.
#[test]
fn undo_after_a_failed_exec_gives_every_window_its_marks_back() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", false);
    world.host.add_shell("ws4-002", false);
    let a = world.sway.open_marked("4", &world.mark("ws4-001"));
    let b = world.sway.open_marked("4", &world.mark("ws4-002"));
    let request = HereRequest {
        shell: Some(t("ws4", 1)),
        terminal: TerminalRequest {
            force: true,
            ..TerminalRequest::default()
        },
        ..HereRequest::default()
    };
    let before = world.store.load().unwrap();
    let exec = here(&world.ports(), b, &request).unwrap();
    assert_eq!(windows_of(&world, "ws4-001"), [b]);

    undo_here(&world.ports(), &exec.changes).unwrap();

    assert_eq!(world.store.load().unwrap(), before);
    assert_eq!(marks_of_window(&world, a), [world.mark("ws4-001")]);
    assert_eq!(marks_of_window(&world, b), [world.mark("ws4-002")]);
    assert_eq!(world.host.shells().len(), 2);
}

/// The undo of a remote `here` leaves no ssh master behind: the binding it
/// made goes, so the host is unused again.
#[test]
fn undo_after_a_failed_exec_stops_the_master_it_left_unused() {
    let world = World::new(deepthought());
    let id = here_terminal(&world, "4");
    let request = HereRequest {
        host: Some(deepthought()),
        ..HereRequest::default()
    };
    let exec = world.finish(|ports| here(ports, id, &request)).unwrap();
    assert!(world.fakes.running(&dt()));

    world
        .finish(|ports| undo_here(ports, &exec.changes))
        .unwrap();

    assert!(!world.fakes.running(&dt()), "{:?}", world.fakes.events());
}

/// Window commands are compared by their words: other spacing or quoting
/// of the same words is the binding's command, not another one.
#[test]
fn here_compares_window_commands_by_their_words() {
    let world = World::new(Host::Local);
    world.bind_with("4", "ws4", "w {shell}");
    let id = here_terminal(&world, "4");
    let request = HereRequest {
        window_command: Some(template("w   '{shell}'")),
        ..HereRequest::default()
    };

    here(&world.ports(), id, &request).unwrap();
}

/// Concurrency (plan rule 4 for `here`): eight terminals in one unbound
/// workspace run `here` at once. The workspace is bound once; each
/// terminal gets its own new shell and exactly one i3mux mark; no shell
/// is shown twice.
#[test]
fn concurrent_heres_bind_once_and_never_share_a_shell() {
    let world = World::new(Host::Local);
    let ids: Vec<u64> = (0..8)
        .map(|n| world.sway.open_titled("4", "foot", &format!("nonce-{n}")))
        .collect();
    let ports = world.ports();

    let titles: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = ids
            .iter()
            .map(|id| {
                let ports = &ports;
                scope.spawn(move || here(ports, *id, &HereRequest::default()).unwrap().title)
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut titles = titles;
    titles.sort();
    let expected: Vec<String> = (1..=8).map(|n| format!("i3mux:local:ws4-{n:03}")).collect();
    assert_eq!(titles, expected);
    assert_eq!(world.window_numbers("ws4"), [1, 2, 3, 4, 5, 6, 7, 8]);
    let tree = world.sway.get_tree().unwrap();
    for id in ids {
        assert_eq!(window::marks_of(&tree, id).len(), 1, "window {id}");
    }
    let state = world.store.load().unwrap();
    assert_eq!(state.bindings().count(), 1);
    assert_eq!(state.get("4").unwrap().session(), &name("ws4"));
}

/// Concurrency: `here` races `i3mux terminal` in one bound workspace with
/// an orphan shell. The orphan goes to exactly one of them, and every
/// window shows its own shell.
#[test]
fn here_racing_terminal_never_picks_the_same_shell() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    world.host.add_shell("ws4-003", false); // an orphan
    let ids: Vec<u64> = (0..4)
        .map(|n| world.sway.open_titled("4", "foot", &format!("nonce-{n}")))
        .collect();
    let ports = world.ports();

    let names: Vec<u32> = std::thread::scope(|scope| {
        let heres: Vec<_> = ids
            .iter()
            .map(|id| {
                let ports = &ports;
                scope.spawn(move || here(ports, *id, &HereRequest::default()).unwrap().title)
            })
            .collect();
        let terminals: Vec<_> = (0..4)
            .map(|_| {
                let ports = &ports;
                scope.spawn(move || {
                    open_terminal(ports, "4", &TerminalRequest::default())
                        .unwrap()
                        .name()
                        .number()
                })
            })
            .collect();
        let mut numbers: Vec<u32> = heres
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap()
                    .rsplit('-')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect();
        numbers.extend(terminals.into_iter().map(|h| h.join().unwrap()));
        numbers
    });

    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 8, "a shell was picked twice: {names:?}");
    assert!(
        sorted.contains(&3),
        "the orphan was not reattached: {names:?}"
    );
    assert_eq!(world.window_numbers("ws4").len(), 8);
}

/// PLAN-terminal-here decision 1: the new shell of a window command comes
/// before the binding and the mark. A host that cannot make it leaves the
/// workspace unbound and the terminal unmarked.
#[test]
fn here_whose_new_shell_fails_binds_and_marks_nothing() {
    let world = World::new(Host::Local);
    world.host.state.lock().unwrap().fail_create = Some("shpool: cannot start the shell".into());
    let id = here_terminal(&world, "4");
    let request = HereRequest {
        window_command: Some(template("w {shell}")),
        ..HereRequest::default()
    };

    here(&world.ports(), id, &request).unwrap_err();

    assert!(world.store.load().unwrap().get("4").is_none(), "bound");
    assert!(world.sway.commands().is_empty(), "marked");
}

/// A move away from a remote host ends the session's shells there, then
/// stops the host's ssh master: no binding uses it and no window is
/// marked with it (plan-ssh-recovery rule 12). Nothing runs there after.
#[test]
fn a_move_away_from_a_remote_host_stops_its_master() {
    let world = World::new(deepthought());
    world.bind("4", "ws4");
    world.host.add_shell("ws4-001", true);
    world.sway.open_marked("4", &world.mark("ws4-001"));
    let (local, hosts) = with_local_host(&world);
    let ports = Ports {
        hosts: hosts.as_ref(),
        ..world.ports()
    };

    with_unused_masters_stopped(&world.store, &world.masters, world.sway.as_ref(), || {
        migrate(&ports, &Host::Local, &name("ws4"), &mut |_| {})
    })
    .unwrap();

    assert!(world.host.shells().is_empty());
    assert_eq!(local.shells().into_keys().collect::<Vec<_>>(), ["ws4-001"]);
    assert_eq!(world.fakes.stopped(), [dt()]);
    assert!(!world.fakes.running(&dt()), "{:?}", world.fakes.events());
    let tree = world.sway.get_tree().unwrap();
    assert_eq!(
        window::session_terminals(&tree, &Host::Local, &name("ws4")),
        [t("ws4", 1)]
    );
}

// ------------------------------------------------------------- activate

#[test]
fn activate_binds_and_opens_the_first_terminal() {
    let world = World::new(Host::Local);
    let (session, choice) =
        activate(&world.ports(), &Host::Local, &workspace("4", 4), None, None).unwrap();
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
    let err = activate(&world.ports(), &remote, &workspace("4", 4), None, None).unwrap_err();
    assert!(err.to_string().contains("cannot reach"), "{err:#}");
    assert_eq!(world.store.load().unwrap().bindings().count(), 0);
}

#[test]
fn activate_named_workspace_uses_its_name_as_session() {
    let world = World::new(Host::Local);
    let (session, _) = activate(
        &world.ports(),
        &Host::Local,
        &workspace("q", -1),
        None,
        None,
    )
    .unwrap();
    assert_eq!(session, name("q"));
}

#[test]
fn activate_numbered_workspace_uses_ws_prefix() {
    let world = World::new(Host::Local);
    let (session, _) =
        activate(&world.ports(), &Host::Local, &workspace("4", 4), None, None).unwrap();
    assert_eq!(session, name("ws4"));
}

#[test]
fn activate_workspace_with_invalid_name_needs_a_session_name() {
    let world = World::new(Host::Local);
    let err = activate(
        &world.ports(),
        &Host::Local,
        &workspace("a:b", -1),
        None,
        None,
    )
    .unwrap_err();
    assert!(
        matches!(command_error(&err), CommandError::NoDefaultSession { workspace, .. } if workspace == "a:b")
    );
    assert!(err.to_string().contains("--session"), "{err:#}");
    let (session, _) = activate(
        &world.ports(),
        &Host::Local,
        &workspace("a:b", -1),
        Some(name("notes")),
        None,
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

// ----------------------------------------------------------------- open

/// The marks of the i3mux windows on `workspace`.
fn marks_on(world: &World, workspace: &str) -> Vec<String> {
    let tree = world.sway.get_tree().unwrap();
    window::find_workspace(&tree, workspace)
        .map(|ws| {
            window::i3mux_windows_in(ws)
                .into_iter()
                .map(|(_, mark)| mark.to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn open_shows_a_shell_in_the_workspace_bound_to_its_session() {
    // A shell that another program made (README, "Shells made by other
    // programs"): the window attaches it as it is, without a command, and
    // lands in the bound workspace, not the focused one ("4").
    let world = World::new(deepthought());
    world.bind("9", "ws9");
    world.host.add_shell("ws9-003", false);

    let opened = world
        .finish(|ports| open(ports, &deepthought(), &t("ws9", 3), false))
        .unwrap();

    assert_eq!(opened, Opened::Window("9".to_owned()));
    assert_eq!(marks_on(&world, "9"), [world.mark("ws9-003").to_string()]);
    assert!(marks_on(&world, "4").is_empty());
    let launch = &world.launcher.argvs()[0];
    assert_eq!(launch[launch.len() - 2..], ["attach", "ws9-003"]);
    assert!(
        world.fakes.running(&dt()),
        "the bound host keeps its master"
    );
}

#[test]
fn open_of_a_shell_that_has_a_window_opens_no_second_one() {
    let world = World::new(Host::Local);
    world.bind("9", "ws9");
    world.host.add_shell("ws9-003", false);
    let ports = world.ports();

    let first = open(&ports, &Host::Local, &t("ws9", 3), false).unwrap();
    let again = open(&ports, &Host::Local, &t("ws9", 3), false).unwrap();

    assert_eq!(first, Opened::Window("9".to_owned()));
    assert_eq!(again, Opened::AlreadyOpen("9".to_owned()));
    assert_eq!(world.launcher.argvs().len(), 1);
}

#[test]
fn open_of_a_session_bound_nowhere_is_refused_without_asking_the_host() {
    let world = World::new(deepthought());
    world.host.add_shell("ws9-003", false);

    let err = world
        .finish(|ports| open(ports, &deepthought(), &t("ws9", 3), false))
        .unwrap_err();

    assert!(
        matches!(command_error(&err), CommandError::NotBound { .. }),
        "{err:#}"
    );
    assert_eq!(
        err.to_string(),
        "No workspace is bound to session 'ws9' on deepthought; run 'i3mux activate --remote \
         deepthought --session ws9' in the workspace that should show it"
    );
    assert_eq!(failure_status(&err), EXIT_NO_BINDING);
    assert!(
        world.fakes.events().is_empty(),
        "{:?}",
        world.fakes.events()
    );
    assert!(world.launcher.argvs().is_empty());
}

#[test]
fn open_never_creates_a_shell() {
    // `shpool attach` would create a missing shell: open refuses first.
    let world = World::new(Host::Local);
    world.bind("9", "ws9");
    world.host.add_shell("ws9-001", false);

    let err = open(&world.ports(), &Host::Local, &t("ws9", 3), false).unwrap_err();

    assert_eq!(
        err.to_string(),
        "No shell 'ws9-003' on local; run 'i3mux sessions' to list sessions"
    );
    assert!(world.launcher.argvs().is_empty());
    assert_eq!(world.host.shells().len(), 1);
}

#[test]
fn open_takes_over_a_shell_attached_elsewhere_only_with_force() {
    let world = World::new(Host::Local);
    world.bind("9", "ws9");
    world.host.add_shell("ws9-003", true);

    let err = open(&world.ports(), &Host::Local, &t("ws9", 3), false).unwrap_err();
    assert!(
        matches!(command_error(&err), CommandError::AlreadyAttached(_)),
        "{err:#}"
    );
    assert!(world.launcher.argvs().is_empty());

    let opened = open(&world.ports(), &Host::Local, &t("ws9", 3), true).unwrap();
    assert_eq!(opened, Opened::Window("9".to_owned()));
    let launch = &world.launcher.argvs()[0];
    assert_eq!(launch[launch.len() - 3..], ["attach", "-f", "ws9-003"]);
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
            cleanup: None,
        }),
        row(RowStatus::Bound {
            workspace: "mail".into(),
            windows: 1,
            orphans: 0,
            cleanup: None,
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

/// PLAN-terminal-here decision 4: `terminal --no-hold` asks the wrapper to
/// end the window with its shell; other windows keep today's hold.
#[test]
fn a_no_hold_terminal_tells_its_wrapper_not_to_hold() {
    let world = World::new(Host::Local);
    world.bind("4", "ws4");
    let request = TerminalRequest {
        no_hold: true,
        ..TerminalRequest::default()
    };
    open_terminal(&world.ports(), "4", &request).unwrap();
    open_terminal(&world.ports(), "4", &TerminalRequest::default()).unwrap();

    let options = world.launcher.options.lock().unwrap().clone();
    assert_eq!(
        options,
        [
            WrapperOptions {
                no_hold: true,
                attach_only: false
            },
            WrapperOptions::default()
        ]
    );
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
    assert_eq!(world.window_numbers("ws4"), [1], "marked with its host");
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

// PLAN-terminal-here decision 5: a window marked with a host keeps the
// host's master, bound or not.

#[test]
fn a_master_stays_while_a_window_is_marked_with_its_host() {
    // No workspace is bound to deepthought, but a window still shows one of
    // its shells: listing its sessions must not cut that window's link.
    let world = World::new(deepthought());
    world.host.add_shell("ws4-001", true);
    world.sway.open_marked("4", &world.mark("ws4-001"));

    world
        .finish(|ports| sessions(ports, &deepthought()))
        .unwrap();

    assert!(
        world.fakes.stopped().is_empty(),
        "{:?}",
        world.fakes.events()
    );
    assert!(world.fakes.running(&dt()));
}

#[test]
fn the_master_stops_once_no_window_is_marked_with_its_host() {
    let world = World::new(deepthought());
    world.host.add_shell("ws4-001", false);
    world.sway.open("4", "foot");

    world
        .finish(|ports| sessions(ports, &deepthought()))
        .unwrap();

    assert_eq!(
        world.fakes.stopped(),
        [dt()],
        "an unmarked window keeps nothing"
    );
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

/// `--migrate` names its session and takes no `--force`; `--rebind-only`
/// is gone (PLAN-migrate: it replaces it).
#[test]
fn migrate_needs_a_session_and_takes_no_force() {
    let parse = |args: &[&str]| Cli::try_parse_from([&["i3mux", "attach"], args].concat());
    assert!(parse(&["--migrate", "--remote", "h"]).is_err());
    assert!(parse(&["--migrate", "--session", "s", "--force"]).is_err());
    assert!(parse(&["--migrate", "--remote", "h", "--session", "s"]).is_ok());
    assert!(parse(&["--migrate", "--session", "s"]).is_ok());
    assert!(parse(&["--rebind-only", "--remote", "h", "--session", "s"]).is_err());
}

/// `--shell`, `--remote` and `--window-command` belong to `--here`, and
/// `--shell` (attach only) takes no `-e` and no `--new`.
#[test]
fn here_options_need_here_and_shell_takes_no_new_shell() {
    let parse = |args: &[&str]| Cli::try_parse_from([&["i3mux", "terminal"], args].concat());
    for alone in [
        ["--shell", "ws4-001"],
        ["--remote", "h"],
        ["--window-command", "w"],
    ] {
        assert!(parse(&alone).is_err(), "{alone:?}");
    }
    assert!(parse(&["--here", "--shell", "ws4-001", "-e", "htop"]).is_err());
    assert!(parse(&["--here", "--shell", "ws4-001", "--new"]).is_err());
    assert!(parse(&[
        "--here",
        "-r",
        "h",
        "--shell",
        "ws4-001",
        "--force",
        "--no-hold"
    ])
    .is_ok());
    assert!(parse(&["--here", "--window-command", "w {shell}", "-e", "htop"]).is_ok());
}

/// `reattach` is hidden: it is the wrapper's, not the user's.
#[test]
fn reattach_is_a_hidden_command_of_the_wrapper() {
    use clap::CommandFactory;
    let parsed = Cli::try_parse_from([
        "i3mux",
        "reattach",
        "--no-hold",
        "--shell",
        "ws4-002",
        "--from",
        "local",
    ]);
    assert!(parsed.is_ok());
    assert!(Cli::try_parse_from(["i3mux", "reattach", "--shell", "ws4-002"]).is_err());
    let help = Cli::command().render_help().to_string();
    assert!(!help.contains("reattach"), "{help}");
}

#[test]
fn kill_remote_requires_session() {
    assert!(Cli::try_parse_from(["i3mux", "kill", "--remote", "h"]).is_err());
    assert!(Cli::try_parse_from(["i3mux", "kill", "--remote", "h", "--session", "s"]).is_ok());
    assert!(Cli::try_parse_from(["i3mux", "kill"]).is_ok());
}
