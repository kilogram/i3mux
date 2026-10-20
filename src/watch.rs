//! The layout watcher, `i3mux watch` (plan-layout-backup decision 3). It
//! runs as a user unit beside one Sway, hears Sway's events, and keeps this
//! machine's backup of every binding that this Sway owns.
//!
//! Guarantees:
//!
//! - It captures the current tree [`SETTLE`] after the first event of a
//!   burst, and never builds a layout from an event's contents (rule 7).
//!   Its own start counts as the first event of a burst, so windows that
//!   opened before it subscribed are backed up too.
//! - It writes a backup only for a binding that its own Sway owns and that
//!   has at least one i3mux window in its workspace, and only under the
//!   state lock (rule 6). So it never sees a `detach`, `kill` or resume
//!   halfway, and never backs up an empty layout. An unchanged layout is not
//!   written again.
//! - On Sway's `shutdown` event it stops at once, without writing (rule 8).
//! - It never contacts a host: this module imports nothing that does
//!   (rule 10, enforced by `just check-boundaries`).
//! - It never changes a binding.

use std::io::{BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::backup::BackupStore;
use crate::layout::{self, Layout};
use crate::state::{LocalState, StateStore, WorkspaceState};
use crate::sway_ops::Sway;
use crate::types::SwayInstance;

/// The Sway events the watcher subscribes to, as `swaymsg` takes them.
pub const SWAY_EVENTS: &str = r#"["window","workspace","shutdown"]"#;

/// How long after the first event of a burst the watcher captures (rule 7).
pub const SETTLE: Duration = Duration::from_secs(1);

/// The next thing the watcher hears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    /// A window or workspace changed. What changed is never used (rule 7).
    Change,
    /// Sway is exiting (rule 8).
    Shutdown,
    /// The event stream ended: Sway closed its socket, or `swaymsg` stopped.
    Closed,
}

/// How a burst ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// The settle time passed. Changes during it were swallowed.
    Quiet,
    /// Sway is exiting (rule 8).
    Shutdown,
    /// The event stream ended.
    Closed,
}

/// Sway's event stream, as the watch loop needs it.
pub trait Events {
    /// Wait for the next event.
    ///
    /// # Errors
    /// If the stream cannot be read or makes no sense.
    fn next(&mut self) -> Result<Heard>;

    /// Let `period` pass, swallowing changes. A shutdown or the end of the
    /// stream ends the wait early.
    ///
    /// # Errors
    /// As [`Events::next`].
    fn settle(&mut self, period: Duration) -> Result<Settled>;
}

/// Why the watcher stopped without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Sway announced its shutdown.
    Shutdown,
    /// The event stream ended.
    Closed,
}

/// The watch loop: settle, `back_up`, wait for the next change, again. It
/// starts with a settle, as if its start were an event.
///
/// # Errors
/// The first error of `events` or `back_up`.
pub fn watch(events: &mut dyn Events, mut back_up: impl FnMut() -> Result<()>) -> Result<Stop> {
    loop {
        match events.settle(SETTLE)? {
            Settled::Quiet => back_up()?,
            Settled::Shutdown => return Ok(Stop::Shutdown),
            Settled::Closed => return Ok(Stop::Closed),
        }
        match events.next()? {
            Heard::Change => {}
            Heard::Shutdown => return Ok(Stop::Shutdown),
            Heard::Closed => return Ok(Stop::Closed),
        }
    }
}

/// The backups to write: for each binding that `sway` owns, the layout of
/// its session's windows in its workspace of `tree`. A binding with no
/// i3mux window there gets none (rule 6).
#[must_use]
pub fn layouts_to_back_up<'a>(
    state: &'a LocalState,
    tree: &Value,
    sway: &SwayInstance,
) -> Vec<(&'a WorkspaceState, Layout)> {
    state
        .bindings()
        .filter(|(_, binding)| binding.is_owned_by(sway))
        .filter_map(|(workspace, binding)| {
            let layout =
                layout::capture_workspace(tree, workspace, binding.host(), binding.session())?;
            Some((binding, layout))
        })
        .collect()
}

/// Under the state lock, capture the tree and write the backups of
/// [`layouts_to_back_up`] whose layout changed (rule 6).
///
/// # Errors
/// If the lock, Sway, or a backup file fails.
pub fn back_up(
    store: &StateStore,
    sway: &dyn Sway,
    backups: &BackupStore,
    current_sway: &SwayInstance,
) -> Result<()> {
    let lock = store.lock()?;
    let tree = sway.get_tree()?;
    for (binding, layout) in layouts_to_back_up(lock.state(), &tree, current_sway) {
        if backups.read(binding)?.as_ref() != Some(&layout) {
            backups.write(binding, &layout)?;
        }
    }
    Ok(())
}

/// After the event stream closed: whether the watcher may exit 0, because
/// Sway closed its socket (rule 17). systemd restarts it after an error.
///
/// # Errors
/// If `swaymsg` ended before Sway confirmed the subscription, or a signal
/// killed it.
pub fn check_closed(subscribed: bool, swaymsg: ExitStatus) -> Result<()> {
    if !subscribed {
        bail!("swaymsg ended before Sway confirmed the subscription ({swaymsg})");
    }
    if let Some(signal) = swaymsg.signal() {
        bail!("swaymsg was killed by signal {signal}");
    }
    Ok(())
}

/// What one JSON value of `swaymsg -r -m -t subscribe` output says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Message {
    /// The reply to the subscription.
    Subscribed,
    /// A window or workspace event.
    Change,
    /// The shutdown event: `{"change": "exit"}`.
    Shutdown,
}

/// Classify one value. Only enough of it is read to tell a shutdown from a
/// change (rule 7).
fn parse_message(value: &Value) -> Result<Message> {
    if let Some(success) = value.get("success") {
        if success.as_bool() == Some(true) {
            return Ok(Message::Subscribed);
        }
        bail!("Sway refused the subscription to its events: {value}");
    }
    let shutdown = value.get("change").and_then(Value::as_str) == Some("exit")
        && value.get("container").is_none()
        && value.get("current").is_none();
    Ok(if shutdown {
        Message::Shutdown
    } else {
        Message::Change
    })
}

/// One JSON value of `swaymsg` output, or why it could not be read.
type OutputValue = Result<Value, serde_json::Error>;

/// [`Events`] from the output of `swaymsg -m`, which another thread reads.
/// The output is a stream of JSON values, one per event; `swaymsg` may
/// print each on one line or on several.
pub struct SwaymsgEvents {
    values: Receiver<OutputValue>,
    subscribed: bool,
}

impl SwaymsgEvents {
    /// Events from the JSON values of `output`. A thread reads them until
    /// the end of `output`, or the first value that is not JSON.
    #[must_use]
    pub fn read(output: impl Read + Send + 'static) -> Self {
        let (sender, values) = mpsc::channel();
        std::thread::spawn(move || {
            let stream = serde_json::Deserializer::from_reader(BufReader::new(output));
            for value in stream.into_iter::<Value>() {
                let failed = value.is_err();
                if sender.send(value).is_err() || failed {
                    break;
                }
            }
        });
        Self::from_values(values)
    }

    fn from_values(values: Receiver<OutputValue>) -> Self {
        Self {
            values,
            subscribed: false,
        }
    }

    /// Whether Sway confirmed the subscription.
    #[must_use]
    pub fn subscribed(&self) -> bool {
        self.subscribed
    }

    fn classify(&mut self, value: OutputValue) -> Result<Message> {
        let message = parse_message(&value.context("cannot read the output of swaymsg")?)?;
        if message == Message::Subscribed {
            self.subscribed = true;
        }
        Ok(message)
    }
}

impl Events for SwaymsgEvents {
    fn next(&mut self) -> Result<Heard> {
        loop {
            let Ok(value) = self.values.recv() else {
                return Ok(Heard::Closed);
            };
            match self.classify(value)? {
                Message::Subscribed => {}
                Message::Change => return Ok(Heard::Change),
                Message::Shutdown => return Ok(Heard::Shutdown),
            }
        }
    }

    fn settle(&mut self, period: Duration) -> Result<Settled> {
        let deadline = Instant::now() + period;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let value = match self.values.recv_timeout(left) {
                Ok(value) => value,
                Err(RecvTimeoutError::Timeout) => return Ok(Settled::Quiet),
                Err(RecvTimeoutError::Disconnected) => return Ok(Settled::Closed),
            };
            match self.classify(value)? {
                Message::Subscribed | Message::Change => {}
                Message::Shutdown => return Ok(Settled::Shutdown),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateStore;
    use crate::sway_ops::testing::FakeSway;
    use crate::types::{Host, SessionName, TerminalName, Token};
    use crate::window::Mark;
    use std::collections::VecDeque;
    use std::sync::mpsc::Sender;

    const SUBSCRIBED: &str = r#"{"success": true}"#;
    const WINDOW_EVENT: &str = r#"{"change": "new", "container": {"id": 7}}"#;
    const WORKSPACE_EVENT: &str = r#"{"change": "focus", "current": {}, "old": null}"#;
    const SHUTDOWN_EVENT: &str = r#"{"change": "exit"}"#;

    /// Events from two scripts, one per method.
    #[derive(Default)]
    struct Script {
        settles: VecDeque<Settled>,
        nexts: VecDeque<Heard>,
    }

    impl Events for Script {
        fn next(&mut self) -> Result<Heard> {
            Ok(self.nexts.pop_front().expect("script has no more events"))
        }
        fn settle(&mut self, period: Duration) -> Result<Settled> {
            assert_eq!(period, SETTLE, "rule 7: one second");
            Ok(self
                .settles
                .pop_front()
                .expect("script has no more settles"))
        }
    }

    fn script(settles: &[Settled], nexts: &[Heard]) -> Script {
        Script {
            settles: settles.iter().copied().collect(),
            nexts: nexts.iter().copied().collect(),
        }
    }

    /// Run [`watch`] over `events`; return its stop and the number of
    /// backups it made.
    fn run(mut events: Script) -> (Stop, usize) {
        let mut backups = 0;
        let stop = watch(&mut events, || {
            backups += 1;
            Ok(())
        })
        .unwrap();
        (stop, backups)
    }

    #[test]
    fn the_start_and_each_burst_back_up_once() {
        // Rule 7: one capture per burst, after it settles.
        let events = script(
            &[Settled::Quiet, Settled::Quiet, Settled::Quiet],
            &[Heard::Change, Heard::Change, Heard::Closed],
        );
        assert_eq!(run(events), (Stop::Closed, 3));
    }

    #[test]
    fn shutdown_during_a_burst_stops_without_backing_up() {
        // Rule 8.
        let events = script(&[Settled::Quiet, Settled::Shutdown], &[Heard::Change]);
        assert_eq!(run(events), (Stop::Shutdown, 1));
        let events = script(&[Settled::Shutdown], &[]);
        assert_eq!(run(events), (Stop::Shutdown, 0));
    }

    #[test]
    fn shutdown_or_close_between_bursts_stops() {
        assert_eq!(
            run(script(&[Settled::Quiet], &[Heard::Shutdown])),
            (Stop::Shutdown, 1)
        );
        assert_eq!(run(script(&[Settled::Closed], &[])), (Stop::Closed, 0));
    }

    #[test]
    fn a_failed_backup_stops_the_watcher() {
        let mut events = script(&[Settled::Quiet], &[]);
        let err = watch(&mut events, || anyhow::bail!("disk full")).unwrap_err();
        assert_eq!(err.to_string(), "disk full");
    }

    fn parse(text: &str) -> Result<Message> {
        parse_message(&serde_json::from_str(text).unwrap())
    }

    #[test]
    fn values_are_told_apart_by_little_more_than_the_shutdown() {
        assert_eq!(parse(SUBSCRIBED).unwrap(), Message::Subscribed);
        assert_eq!(parse(WINDOW_EVENT).unwrap(), Message::Change);
        assert_eq!(parse(WORKSPACE_EVENT).unwrap(), Message::Change);
        assert_eq!(parse(SHUTDOWN_EVENT).unwrap(), Message::Shutdown);
        let exit_of_a_window = r#"{"change": "exit", "container": {}}"#;
        assert_eq!(parse(exit_of_a_window).unwrap(), Message::Change);
        assert!(parse(r#"{"success": false}"#).is_err());
    }

    fn channel() -> (Sender<OutputValue>, SwaymsgEvents) {
        let (sender, values) = mpsc::channel();
        (sender, SwaymsgEvents::from_values(values))
    }

    fn send(sender: &Sender<OutputValue>, text: &str) {
        sender
            .send(Ok(serde_json::from_str(text).unwrap()))
            .unwrap();
    }

    #[test]
    fn a_settle_swallows_the_burst_and_lasts_its_period() {
        let (sender, mut events) = channel();
        send(&sender, SUBSCRIBED);
        send(&sender, WINDOW_EVENT);
        assert_eq!(events.next().unwrap(), Heard::Change);
        assert!(events.subscribed());

        let feeder_sender = sender.clone();
        let feeder = std::thread::spawn(move || {
            for _ in 0..3 {
                std::thread::sleep(Duration::from_millis(20));
                send(&feeder_sender, WINDOW_EVENT);
            }
        });
        let started = Instant::now();
        assert_eq!(
            events.settle(Duration::from_millis(200)).unwrap(),
            Settled::Quiet
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
        feeder.join().unwrap();
        drop(sender);
        assert_eq!(
            events.next().unwrap(),
            Heard::Closed,
            "the burst's events are gone"
        );
    }

    #[test]
    fn shutdown_ends_a_settle_at_once() {
        let (sender, mut events) = channel();
        send(&sender, WINDOW_EVENT);
        send(&sender, SHUTDOWN_EVENT);
        let started = Instant::now();
        assert_eq!(
            events.settle(Duration::from_secs(30)).unwrap(),
            Settled::Shutdown
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn the_end_of_the_stream_ends_a_settle_and_a_wait() {
        let (sender, mut events) = channel();
        drop(sender);
        assert_eq!(
            events.settle(Duration::from_secs(30)).unwrap(),
            Settled::Closed
        );
        assert_eq!(events.next().unwrap(), Heard::Closed);
        assert!(!events.subscribed());
    }

    #[test]
    fn events_are_read_value_by_value_on_one_line_or_several() {
        let pretty = "{\n  \"change\": \"close\",\n  \"container\": {\n    \"id\": 3\n  }\n}";
        let output = format!("{SUBSCRIBED}\n{WINDOW_EVENT}\n{pretty}\n{SHUTDOWN_EVENT}\n");
        let mut events = SwaymsgEvents::read(std::io::Cursor::new(output.into_bytes()));
        assert_eq!(events.next().unwrap(), Heard::Change);
        assert_eq!(events.next().unwrap(), Heard::Change);
        assert_eq!(events.next().unwrap(), Heard::Shutdown);
        assert_eq!(events.next().unwrap(), Heard::Closed);
        assert!(events.subscribed());
    }

    #[test]
    fn output_that_is_not_json_is_an_error() {
        let output = format!("{SUBSCRIBED}\nnot json\n{WINDOW_EVENT}\n");
        let mut events = SwaymsgEvents::read(std::io::Cursor::new(output.into_bytes()));
        assert!(events.next().is_err());
    }

    #[test]
    fn a_closed_stream_is_clean_only_after_the_subscription() {
        // Rule 17: exit 0 when Sway closes its socket.
        let exited = |code: i32| ExitStatus::from_raw(code << 8);
        check_closed(true, exited(1)).unwrap();
        check_closed(true, exited(0)).unwrap();
        assert!(check_closed(false, exited(1)).is_err());
        let killed = ExitStatus::from_raw(9);
        assert!(check_closed(true, killed).is_err());
    }

    // ------------------------------------------------- the write decision

    fn name(text: &str) -> SessionName {
        SessionName::new(text).unwrap()
    }

    fn this_sway() -> SwayInstance {
        SwayInstance::new("boot", "/run/user/1000/sway-ipc.1000.2.sock")
    }

    fn other_sway() -> SwayInstance {
        SwayInstance::new("boot", "/run/user/1000/sway-ipc.1000.1.sock")
    }

    fn token() -> Token {
        Token::parse(&"a".repeat(32)).unwrap()
    }

    fn binding(session: &str, owner: &SwayInstance) -> WorkspaceState {
        WorkspaceState::new(Host::Local, name(session), token(), owner.clone())
    }

    fn mark(terminal: &str) -> Mark {
        Mark::new(Host::Local, terminal.parse::<TerminalName>().unwrap())
    }

    #[test]
    fn only_bindings_of_this_sway_with_a_window_are_backed_up() {
        // Rule 6.
        let sway = FakeSway::default();
        sway.open_marked("4", &mark("ws4-001"));
        sway.open_marked("4", &mark("ws4-002"));
        sway.open_marked("5", &mark("ws5-001"));
        sway.open_marked("7", &mark("ws7-001"));
        sway.open("6", "firefox");
        let mut state = LocalState::default();
        state.bind("4", binding("ws4", &this_sway())).unwrap();
        state.bind("5", binding("ws5", &other_sway())).unwrap();
        state.bind("6", binding("ws6", &this_sway())).unwrap();
        state
            .bind("7", WorkspaceState::without_token(Host::Local, name("ws7")))
            .unwrap();

        let tree = sway.get_tree().unwrap();
        let backups = layouts_to_back_up(&state, &tree, &this_sway());

        let backed_up: Vec<(&str, usize)> = backups
            .iter()
            .map(|(b, layout)| (b.session().as_str(), layout.terminals().len()))
            .collect();
        assert_eq!(backed_up, [("ws4", 2)]);
    }

    struct Files {
        dir: tempfile::TempDir,
        store: StateStore,
        backups: BackupStore,
    }

    fn files() -> Files {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        let backups = BackupStore::new(dir.path().join("backups"));
        Files {
            dir,
            store,
            backups,
        }
    }

    #[test]
    fn back_up_writes_only_a_changed_layout() {
        use std::os::unix::fs::PermissionsExt;
        let files = files();
        let owned = binding("ws4", &this_sway());
        files
            .store
            .with_lock(|s| Ok(s.bind("4", owned.clone())?))
            .unwrap();
        let sway = FakeSway::default();
        sway.open_marked("4", &mark("ws4-001"));

        back_up(&files.store, &sway, &files.backups, &this_sway()).unwrap();
        let first = files.backups.read(&owned).unwrap().unwrap();
        assert_eq!(first.terminals().len(), 1);

        // A read-only directory fails any write: an unchanged layout must
        // not try one, a changed layout must.
        let dir = files.dir.path().join("backups/local");
        let mode = |m| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(&dir, mode(0o500)).unwrap();
        let unchanged = back_up(&files.store, &sway, &files.backups, &this_sway());
        sway.open_marked("4", &mark("ws4-002"));
        let changed = back_up(&files.store, &sway, &files.backups, &this_sway());
        std::fs::set_permissions(&dir, mode(0o700)).unwrap();
        unchanged.unwrap();
        assert!(changed.is_err(), "a changed layout is written");
        assert_eq!(
            files.store.load().unwrap().get("4"),
            Some(&owned),
            "the watcher never changes a binding"
        );
    }

    #[test]
    fn back_up_waits_for_the_state_lock() {
        // Rule 6: a detach or kill holds the lock; the watcher waits.
        let files = std::sync::Arc::new(files());
        let lock = files.store.lock().unwrap();
        let (done, finished) = mpsc::channel();
        let shared = std::sync::Arc::clone(&files);
        std::thread::spawn(move || {
            let sway = FakeSway::default();
            let result = back_up(&shared.store, &sway, &shared.backups, &this_sway());
            done.send(result.is_ok()).unwrap();
        });
        let wait = Duration::from_millis(300);
        assert!(finished.recv_timeout(wait).is_err(), "ran without the lock");
        drop(lock);
        assert!(finished.recv_timeout(Duration::from_secs(10)).unwrap());
    }
}
