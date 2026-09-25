//! `i3mux sessions` output and `i3mux kill` (plan decisions 7 and 15,
//! "User-facing text").

use rstest::rstest;

use super::common::{terminal, wait_until, Result, Session, TestEnvironment};

const WS: u32 = 4;
const NAME: &str = "ws4";

/// One line per state: bound (windows, orphans), detached (saved count),
/// running shells with no saved layout, and an unreadable saved layout.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn sessions_describes_each_state(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    let host = session.host_label();
    let line = |status: &str| vec![format!("{NAME}  ({host})  {status}")];
    env.activate(session, WS)?;
    env.i3mux().terminal("")?;
    assert_eq!(env.i3mux().sessions(session)?, line("2 windows"));

    env.close_window(&session.mark(NAME, 2))?;
    let closed = terminal(NAME, 2);
    wait_until("the closed window's shell is disconnected", || {
        Ok(env.shell(session, &closed)?.is_some_and(|s| !s.attached))
    })?;
    assert_eq!(env.i3mux().sessions(session)?, line("1 window, 1 orphan"));

    env.i3mux().terminal("")?;
    // Right after detach: detach waits until the closed windows' shells
    // have no client, so the row never says "attached elsewhere" here.
    env.i3mux().detach()?;
    assert_eq!(
        env.i3mux().sessions(session)?,
        line("detached, 2 terminals saved")
    );

    let layout = format!(".local/state/i3mux/sessions/{NAME}.json");
    env.remove_host_file(session, &layout)?;
    assert_eq!(
        env.i3mux().sessions(session)?,
        line("2 shells, no saved layout")
    );

    // A corrupt saved layout is one row, not an error for the whole list.
    env.write_host_file(session, &layout, "{not json")?;
    assert_eq!(
        env.i3mux().sessions(session)?,
        line("unreadable saved layout")
    );
    Ok(())
}

/// `kill` on a bound workspace closes its windows, ends its shells and
/// unbinds it; `sessions` no longer lists it.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn kill_ends_a_bound_session(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    let host = session.host_label();
    env.activate(session, WS)?;
    env.i3mux().terminal("")?;

    let out = env.i3mux().kill("")?.ok()?;
    assert!(
        out.contains(&format!(
            "Killed session '{NAME}' on {host}: 2 shells, 2 windows"
        )),
        "{out}"
    );
    env.wait_for_window_count(WS, 0)?;
    wait_until("the shells are gone", || {
        Ok(env.shells(session)?.is_empty())
    })?;
    assert!(env.i3mux().sessions(session)?.is_empty());
    let stderr = env.i3mux().run("detach")?.failed_with(1)?;
    assert!(stderr.contains("is not bound to a session"), "{stderr}");
    Ok(())
}

/// `kill --session` on a detached session ends its shells and deletes its
/// saved layout; a second kill reports that the session does not exist.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn kill_ends_a_detached_session_and_its_saved_layout(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    let host = session.host_label();
    let layout = format!(".local/state/i3mux/sessions/{NAME}.json");
    env.activate(session, WS)?;
    env.i3mux().detach()?;
    assert!(env.host_file(session, &layout)?.is_some());

    let args = format!("--session {NAME}{}", session.remote_flag());
    let out = env.i3mux().kill(&args)?.ok()?;
    assert!(
        out.contains(&format!(
            "Killed session '{NAME}' on {host}: 1 shell, 0 windows, saved layout deleted"
        )),
        "{out}"
    );
    assert!(env.host_file(session, &layout)?.is_none());
    wait_until("the shells are gone", || {
        Ok(env.shells(session)?.is_empty())
    })?;
    assert!(env.i3mux().sessions(session)?.is_empty());

    let stderr = env.i3mux().kill(&args)?.failed_with(1)?;
    assert!(
        stderr.contains(&format!("No session '{NAME}' on {host}")),
        "{stderr}"
    );
    Ok(())
}
