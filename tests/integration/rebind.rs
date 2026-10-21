//! `attach --rebind-only` (README, "Move a binding to another host";
//! PLAN-terminal-here decision 3): the binding and the window marks move to
//! the other host, and nothing else changes.

use super::common::{wait_until, Result, Session, TestEnvironment};

const WS: u32 = 4;
const NAME: &str = "ws4";

/// A remote session's binding moves to this machine and back. Each time
/// the windows stay open with the same shells attached (their processes
/// are not touched), only their marks name the new host, and `status`
/// shows it; a second run changes nothing.
#[test]
fn rebind_only_moves_the_binding_and_the_marks() -> Result<()> {
    let env = TestEnvironment::new()?;
    env.activate(Session::Remote, WS)?;
    env.i3mux().terminal("")?;
    wait_until("both windows attached their shells", || {
        let shells = env.shells(Session::Remote)?;
        Ok(shells.len() == 2 && shells.iter().all(|s| s.attached))
    })?;
    let shells = env.shells(Session::Remote)?;
    let pids: Vec<u64> = shells.iter().map(|s| s.pid).collect();

    let moved = env
        .i3mux()
        .run(&format!("attach --session {NAME} --rebind-only"))?
        .ok()?;

    assert!(
        moved.contains("moved from i3mux-remote-ssh on local; 2 windows re-marked"),
        "{moved}"
    );
    assert_eq!(
        env.marks(WS)?,
        [Session::Local.mark(NAME, 1), Session::Local.mark(NAME, 2)]
    );
    assert_eq!(env.windows(WS)?.len(), 2, "no window opened or closed");
    let after = env.shells(Session::Remote)?;
    assert_eq!(after.iter().map(|s| s.pid).collect::<Vec<_>>(), pids);
    assert!(
        after.iter().all(|s| s.attached),
        "the windows still show them"
    );
    let status = env.i3mux().run("status")?.ok()?;
    assert!(status.starts_with(&format!("{NAME}  (local)")), "{status}");

    let again = env
        .i3mux()
        .run(&format!("attach --session {NAME} --rebind-only"))?
        .ok()?;
    assert!(
        again.contains("already on local; 0 windows re-marked"),
        "{again}"
    );

    env.i3mux()
        .run(&format!(
            "attach{} --session {NAME} --rebind-only",
            Session::Remote.remote_flag()
        ))?
        .ok()?;
    assert_eq!(
        env.marks(WS)?,
        [Session::Remote.mark(NAME, 1), Session::Remote.mark(NAME, 2)]
    );
    Ok(())
}
