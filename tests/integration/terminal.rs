//! Shells outlive their windows (plan decisions 5, 14 and 17), and their
//! output survives detach and attach (findings 1, 2 and 4).

use rstest::rstest;

use super::common::{terminal, wait_until, ComparisonSpec, Result, Session, TestEnvironment};

const WS: u32 = 4;
const NAME: &str = "ws4";

/// Closing a window keeps its shell; the next `i3mux terminal` shows that
/// same shell again under the same mark, and no new number is used.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn next_terminal_reattaches_the_shell_of_a_closed_window(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    env.activate(session, WS)?;
    env.i3mux().terminal("")?;
    let marks = env.marks(WS)?;
    assert_eq!(marks, [session.mark(NAME, 1), session.mark(NAME, 2)]);
    let first = terminal(NAME, 1);
    let pid = env.shell(session, &first)?.unwrap().pid;

    env.close_window(&session.mark(NAME, 1))?;
    wait_until("the shell of the closed window is disconnected", || {
        Ok(env.shell(session, &first)?.is_some_and(|s| !s.attached))
    })?;
    assert_eq!(env.marks(WS)?, [session.mark(NAME, 2)]);

    env.i3mux().terminal("")?;
    assert_eq!(
        env.marks(WS)?,
        marks,
        "the orphan must come back, not a new number"
    );
    let shells = env.shells(session)?;
    let names: Vec<&str> = shells.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["ws4-001", "ws4-002"]);
    assert_eq!(
        env.shell(session, &first)?.unwrap().pid,
        pid,
        "shell replaced"
    );
    Ok(())
}

/// `terminal --new` starts a new shell even when an orphan exists.
#[test]
fn terminal_new_skips_orphans() -> Result<()> {
    let session = Session::Local;
    let env = TestEnvironment::new()?;
    env.activate(session, WS)?;
    env.close_window(&session.mark(NAME, 1))?;
    env.wait_until_all_disconnected(session)?;

    env.i3mux().terminal("--new")?;
    assert_eq!(env.marks(WS)?, [session.mark(NAME, 2)]);
    let shells = env.shells(session)?;
    let names: Vec<&str> = shells.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["ws4-001", "ws4-002"], "the orphan must stay alive");
    Ok(())
}

/// Output printed before detach is on screen again after attach: the
/// window shows the last of 300 lines (golden), from the same process.
/// The pty test `mux::pty_tests` pins that the whole history is replayed.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn scrollback_is_on_screen_again_after_detach_and_attach(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    env.activate(session, WS)?;
    env.i3mux()
        .terminal("-e /opt/i3mux-test/color-scripts/print-lines.sh")?;
    // Leave the printing terminal alone on the workspace.
    env.close_window(&session.mark(NAME, 1))?;
    wait_until("the prompt window is closed", || {
        Ok(env.marks(WS)? == [session.mark(NAME, 2)])
    })?;
    let printer = terminal(NAME, 2);
    let pid = env.shell(session, &printer)?.unwrap().pid;

    env.i3mux().detach()?;
    env.i3mux().attach(session, "")?.ok()?;
    assert_eq!(env.marks(WS)?, [session.mark(NAME, 2)]);
    assert_eq!(env.shell(session, &printer)?.unwrap().pid, pid);

    env.compare_with_golden(
        "scrollback-after-attach.png",
        &env.capture_screenshot()?,
        &ComparisonSpec::load("scrollback-after-attach")?,
    )
}

/// A remote shell survives its window closing: the ssh connection ends,
/// the shell stays alive on the remote, and the workspace stays bound with
/// no windows (decision 17).
#[test]
fn remote_shell_survives_its_window_closing() -> Result<()> {
    let session = Session::Remote;
    let env = TestEnvironment::new()?;
    env.activate(session, WS)?;
    let name = terminal(NAME, 1);
    let pid = env.shell(session, &name)?.unwrap().pid;

    env.close_window(&session.mark(NAME, 1))?;
    wait_until("the remote shell is disconnected", || {
        Ok(env.shell(session, &name)?.is_some_and(|s| !s.attached))
    })?;
    // Give a dying shell time to go before looking again.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let shell = env.shell(session, &name)?;
    assert_eq!(shell.map(|s| s.pid), Some(pid), "the remote shell died");
    assert_eq!(
        env.i3mux().sessions(session)?,
        [format!(
            "{NAME}  ({})  0 windows, 1 orphan",
            session.host_label()
        )]
    );
    Ok(())
}
