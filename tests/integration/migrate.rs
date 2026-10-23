//! `attach --migrate` (README, "Move a session to another host";
//! docs/PLAN-migrate.md rule 4): a session with two windows, one of them
//! running a job, moves to the other host and back. No window closes, and
//! each time both windows attach the new host's shells.

use super::common::{terminal, wait_until, Result, Session, TestEnvironment};

const WS: u32 = 4;
const NAME: &str = "ws4";

/// Wait until the windows of workspace `WS` attach shells 001 and 002 of
/// `NAME` on `session`'s host: both shells exist there, both have a
/// client, and both windows are marked with that host.
fn wait_for_windows_on(env: &TestEnvironment, session: Session) -> Result<()> {
    wait_until(
        &format!(
            "both windows attach their shells on {}",
            session.host_label()
        ),
        || {
            let shells = env.shells(session)?;
            let ours: Vec<_> = shells
                .iter()
                .filter(|s| s.name.starts_with(&format!("{NAME}-")))
                .collect();
            let names: Vec<&str> = ours.iter().map(|s| s.name.as_str()).collect();
            Ok(names == [terminal(NAME, 1), terminal(NAME, 2)] && ours.iter().all(|s| s.attached))
        },
    )?;
    assert_eq!(
        env.marks(WS)?,
        [session.mark(NAME, 1), session.mark(NAME, 2)]
    );
    Ok(())
}

/// The shells of `NAME` on `session`'s host.
fn session_shells(env: &TestEnvironment, session: Session) -> Result<Vec<String>> {
    Ok(env
        .shells(session)?
        .into_iter()
        .map(|s| s.name)
        .filter(|name| name.starts_with(&format!("{NAME}-")))
        .collect())
}

#[test]
fn a_session_moves_to_another_host_and_its_windows_follow() -> Result<()> {
    let env = TestEnvironment::new()?;
    env.activate(Session::Remote, WS)?;
    env.i3mux().terminal("-e 'sleep 100000'")?;
    wait_for_windows_on(&env, Session::Remote)?;

    let moved = env
        .i3mux()
        .run(&format!("attach --session {NAME} --migrate"))?;

    assert!(moved.success(), "{moved}");
    assert!(
        moved.stderr.contains(
            "i3mux: moving ws4 from i3mux-remote-ssh to local: 2 of 2 shells start again \
             there as login shells in ~; programs running on i3mux-remote-ssh end."
        ),
        "{moved}"
    );
    assert!(
        moved
            .stdout
            .contains("✓ Moved session 'ws4' from i3mux-remote-ssh to local"),
        "{moved}"
    );
    wait_for_windows_on(&env, Session::Local)?;
    assert_eq!(env.windows(WS)?.len(), 2, "no window closed");
    assert!(
        session_shells(&env, Session::Remote)?.is_empty(),
        "the job and the shell on the old host ended"
    );
    let status = env.i3mux().run("status")?.ok()?;
    assert!(status.starts_with(&format!("{NAME}  (local)")), "{status}");
    let again = env
        .i3mux()
        .run(&format!("attach --session {NAME} --migrate"))?
        .ok()?;
    assert_eq!(again.trim(), "ws4 is already on local");

    env.i3mux()
        .run(&format!(
            "attach{} --session {NAME} --migrate",
            Session::Remote.remote_flag()
        ))?
        .ok()?;

    wait_for_windows_on(&env, Session::Remote)?;
    assert_eq!(env.windows(WS)?.len(), 2, "no window closed");
    assert!(session_shells(&env, Session::Local)?.is_empty());
    Ok(())
}
