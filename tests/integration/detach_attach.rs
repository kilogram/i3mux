//! `detach` and `attach` for local and remote sessions (plan decisions 3
//! and 6, rules 5, 6 and 10).

use rstest::rstest;

use super::common::{terminal, wait_until, HostShell, Result, Session, TestEnvironment};

const WS: u32 = 4;
const NAME: &str = "ws4";

fn layout_file(session: &str) -> String {
    format!(".local/state/i3mux/sessions/{session}.json")
}

fn pids(shells: &[HostShell]) -> Vec<(String, u64)> {
    shells.iter().map(|s| (s.name.clone(), s.pid)).collect()
}

/// Detach saves the layout on the host and closes the windows; attach
/// reattaches the very same shells (same pids), nothing recreated.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn attach_brings_back_the_shells_that_detach_left(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    let host = session.host_label();
    env.activate(session, WS)?;
    env.sway("split h")?;
    env.i3mux().terminal("")?;
    let marks = env.marks(WS)?;
    assert_eq!(marks, [session.mark(NAME, 1), session.mark(NAME, 2)]);
    let before = env.shells(session)?;

    let out = env.i3mux().detach()?;
    assert!(
        out.contains(&format!("layout of 2 terminals saved on {host}")),
        "{out}"
    );
    env.wait_for_window_count(WS, 0)?;
    assert!(env.host_file(session, &layout_file(NAME))?.is_some());
    assert_eq!(
        env.i3mux().sessions(session)?,
        [format!("{NAME}  ({host})  detached, 2 terminals saved")]
    );

    let out = env.i3mux().attach(session, "")?.ok()?;
    assert!(out.contains("(2 terminals)"), "{out}");
    assert!(!out.contains("recreated"), "{out}");
    assert_eq!(env.marks(WS)?, marks);
    assert_eq!(
        pids(&env.shells(session)?),
        pids(&before),
        "shells were replaced"
    );
    assert_eq!(
        env.i3mux().sessions(session)?,
        [format!("{NAME}  ({host})  workspace {WS}, 2 windows")]
    );
    Ok(())
}

/// Plan rule 11: when the host lost some shells (a reboot), attach starts
/// fresh ones under the saved names, says how many, and keeps the rest.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn attach_recreates_the_shells_the_host_lost(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    env.activate(session, WS)?;
    env.i3mux().terminal("")?;
    let marks = env.marks(WS)?;
    env.i3mux().detach()?;
    let kept = env.shell(session, &terminal(NAME, 2))?.unwrap().pid;
    env.kill_host_shell(session, &terminal(NAME, 1))?;
    wait_until("the killed shell is gone", || {
        Ok(env.shell(session, &terminal(NAME, 1))?.is_none())
    })?;

    let out = env.i3mux().attach(session, "")?.ok()?;
    assert!(out.contains("recreated 1 of 2 shells"), "{out}");
    assert_eq!(env.marks(WS)?, marks);
    let shells = env.shells(session)?;
    let names: Vec<&str> = shells.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["ws4-001", "ws4-002"]);
    assert_eq!(env.shell(session, &terminal(NAME, 2))?.unwrap().pid, kept);
    Ok(())
}

/// Detach closes only i3mux windows; a plain terminal on the workspace
/// stays, and attach adds the i3mux windows back next to it.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn detach_leaves_other_windows_alone(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    env.activate(session, WS)?;
    env.launch_plain_terminal(WS, "sleep 100000")?;
    assert_eq!(env.windows(WS)?.len(), 2);

    env.i3mux().detach()?;
    env.wait_for_window_count(WS, 1)?;
    let left = env.windows(WS)?;
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].0, "foot", "the plain terminal must stay: {left:?}");

    env.i3mux().attach(session, "")?.ok()?;
    assert_eq!(env.marks(WS)?, [session.mark(NAME, 1)]);
    assert_eq!(env.windows(WS)?.len(), 2);
    Ok(())
}

/// Plan rule 5: detach needs a bound workspace, and says how to bind one.
#[test]
fn detach_of_an_unbound_workspace_fails_with_a_hint() -> Result<()> {
    let env = TestEnvironment::new()?;
    env.focus_workspace(9)?;
    let stderr = env.i3mux().run("detach")?.failed_with(1)?;
    assert!(
        stderr.contains("Workspace 9 is not bound to a session; run 'i3mux activate' first"),
        "{stderr}"
    );
    Ok(())
}

/// Plan rule 6: attach refuses a workspace that has i3mux windows, and the
/// refusal changes nothing: the saved session stays detached and saved.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn attach_refuses_a_workspace_with_i3mux_windows(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    let host = session.host_label();
    env.activate(session, 1)?;
    env.i3mux().detach()?;
    env.activate(session, 2)?;

    let stderr = env
        .i3mux()
        .attach(session, "--session ws1")?
        .failed_with(1)?;
    assert!(
        stderr.contains("Workspace 2 already has i3mux terminals; detach or close them first"),
        "{stderr}"
    );
    assert_eq!(env.marks(2)?, [session.mark("ws2", 1)]);
    assert!(env.host_file(session, &layout_file("ws1"))?.is_some());
    assert!(!env.shell(session, &terminal("ws1", 1))?.unwrap().attached);
    assert_eq!(
        env.i3mux().sessions(session)?,
        [
            format!("ws1  ({host})  detached, 1 terminal saved"),
            format!("ws2  ({host})  workspace 2, 1 window"),
        ]
    );
    Ok(())
}

/// Plan rule 10: a shell attached from another client makes attach fail
/// with the `--force` hint and change nothing; `--force` takes the shell
/// over, disconnects the other client, and keeps the same shells.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn attach_force_takes_over_a_shell_attached_elsewhere(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    let host = session.host_label();
    env.activate(session, WS)?;
    env.i3mux().terminal("")?;
    env.i3mux().detach()?;
    env.attach_second_client(session, &terminal(NAME, 1))?;
    let before = env.shells(session)?;

    let stderr = env.i3mux().attach(session, "")?.failed_with(1)?;
    assert!(
        stderr.contains(
            "Session 'ws4-001' is already attached from another terminal or host; \
             use --force to take over"
        ),
        "{stderr}"
    );
    assert!(
        env.windows(WS)?.is_empty(),
        "a refused attach opened windows"
    );
    assert_eq!(
        env.i3mux().sessions(session)?,
        [format!("{NAME}  ({host})  attached elsewhere, 2 shells")],
        "a refused attach must not bind the workspace"
    );
    assert!(env.second_client_running(session)?);

    let out = env.i3mux().attach(session, "--force")?.ok()?;
    assert!(!out.contains("recreated"), "{out}");
    assert_eq!(
        env.marks(WS)?,
        [session.mark(NAME, 1), session.mark(NAME, 2)]
    );
    wait_until("the other client is disconnected", || {
        Ok(!env.second_client_running(session)?)
    })?;
    assert_eq!(
        pids(&env.shells(session)?),
        pids(&before),
        "shells were replaced"
    );
    Ok(())
}

/// With several saved sessions and no `--session`, attach lists them, exits
/// 2, and opens nothing.
#[test]
fn attach_without_a_name_lists_the_candidates_and_exits_2() -> Result<()> {
    let env = TestEnvironment::new()?;
    for ws in [1, 2] {
        env.activate(Session::Local, ws)?;
        env.i3mux().detach()?;
    }
    env.focus_workspace(3)?;

    let stderr = env.i3mux().attach(Session::Local, "")?.failed_with(2)?;
    assert!(
        stderr.contains(
            "Several saved sessions on local:\n  ws1\n  ws2\nChoose one with --session NAME."
        ),
        "{stderr}"
    );
    assert!(env.windows(3)?.is_empty());

    env.i3mux().attach(Session::Local, "--session ws2")?.ok()?;
    assert_eq!(env.marks(3)?, [Session::Local.mark("ws2", 1)]);
    Ok(())
}

/// Sway closes windows asynchronously, so their shpool clients can outlive
/// `detach`; `attach` trusts `shpool list`. `detach` therefore runs
/// `shpool detach` on the closed windows' shells and waits until none is
/// attached, so an immediate local `attach` is never refused (it was, 4 of 4
/// runs, before that fix).
#[test]
fn local_attach_right_after_detach_is_not_refused() {
    let env = TestEnvironment::new().unwrap();
    env.activate(Session::Local, WS).unwrap();
    env.i3mux().terminal("").unwrap();
    env.i3mux().detach().unwrap();
    let out = env.i3mux().attach(Session::Local, "").unwrap();
    assert!(out.success(), "false refusal right after detach: {out}");
}
