//! `terminal --here` (README, "Join a session from any terminal";
//! PLAN-terminal-here decision 1): a terminal that i3mux did not open
//! becomes a window of its workspace's session, or is refused with nothing
//! changed.

use rstest::rstest;

use super::common::{terminal, wait_until, Result, Session, TestEnvironment};

const WS: u32 = 4;
const NAME: &str = "ws4";

/// Start a plain foot window with app id `tag` on the focused workspace.
/// Once `~/go-{tag}` exists, its script runs `i3mux terminal --here ARGS`
/// as a child, the way a user types it in a shell. Then it writes the exit
/// status to `~/exit-{tag}` and i3mux's stderr to `~/err-{tag}`, and waits.
fn launch_here_terminal(env: &TestEnvironment, tag: &str, args: &str) -> Result<()> {
    let script = format!(
        "while [ ! -e ~/go-{tag} ]; do sleep 0.1; done\n\
         i3mux terminal --here {args} 2> ~/err-{tag}\n\
         echo $? > ~/exit-{tag}\n\
         exec sleep 100000\n"
    );
    env.write_host_file(Session::Local, &format!("{tag}.sh"), &script)?;
    env.launch_plain_terminal(WS, &format!("--app-id {tag} bash /root/{tag}.sh"))
}

/// The plain terminal takes the mark of a new shell, though another
/// terminal has the focus. No window opens. The shell runs `-e`'s command
/// in that terminal, and the binding names the host. When the shell ends,
/// the terminal's own script goes on with the shell's exit status
/// (`--no-hold`: no pause).
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn a_plain_terminal_becomes_a_window_of_the_session(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    env.focus_workspace(WS)?;
    let until_end = r#"-e 'sh -c "while [ ! -e $HOME/end ]; do sleep 0.2; done; exit 3"'"#;
    launch_here_terminal(
        &env,
        "here",
        &format!("--no-hold{} {until_end}", session.remote_flag()),
    )?;
    env.launch_plain_terminal(WS, "--app-id other sleep 100000")?;
    env.write_host_file(Session::Local, "go-here", "")?;

    wait_until("the terminal is marked", || {
        Ok(env.marks(WS)? == [session.mark(NAME, 1)])
    })?;
    let windows = env.windows(WS)?;
    assert_eq!(
        windows,
        [
            ("here".to_owned(), vec![session.mark(NAME, 1)]),
            ("other".to_owned(), vec![])
        ],
        "the mark is on the terminal that ran i3mux, and no window opened"
    );
    wait_until("the terminal attached the shell", || {
        Ok(env
            .shell(session, &terminal(NAME, 1))?
            .is_some_and(|s| s.attached))
    })?;
    let title = format!("i3mux:{}:{}", session.host_label(), terminal(NAME, 1));
    assert_eq!(env.window_title("here")?, Some(title));
    let status = env.i3mux().run("status")?.ok()?;
    assert!(
        status.starts_with(&format!("{NAME}  ({})", session.host_label())),
        "{status}"
    );

    env.write_host_file(session, "end", "")?;
    wait_until("the terminal's script goes on", || {
        Ok(env.host_file(Session::Local, "exit-here")?.is_some())
    })?;
    assert!(env.shell(session, &terminal(NAME, 1))?.is_none());
    assert_eq!(
        env.host_file(Session::Local, "exit-here")?.as_deref(),
        Some("3\n"),
        "the script gets the shell's exit status"
    );
    Ok(())
}

/// A terminal in a workspace bound to this machine asks for the remote
/// host: exit 1, a message that names both hosts, and nothing changes. The
/// terminal gets its own title back.
#[test]
fn a_workspace_bound_to_another_host_is_refused() -> Result<()> {
    let env = TestEnvironment::new()?;
    env.activate(Session::Local, WS)?;
    let before = env.i3mux().run("status")?.ok()?;
    launch_here_terminal(&env, "here", &Session::Remote.remote_flag())?;
    env.write_host_file(Session::Local, "go-here", "")?;

    wait_until("i3mux exits", || {
        Ok(env.host_file(Session::Local, "exit-here")?.is_some())
    })?;
    assert_eq!(
        env.host_file(Session::Local, "exit-here")?.as_deref(),
        Some("1\n")
    );
    let err = env
        .host_file(Session::Local, "err-here")?
        .unwrap_or_default();
    assert!(
        err.contains("bound to session 'ws4' on local, not i3mux-remote-ssh; nothing changed"),
        "{err}"
    );
    assert_eq!(env.marks(WS)?, [Session::Local.mark(NAME, 1)]);
    assert_eq!(env.i3mux().run("status")?.ok()?, before);
    let title = env.window_title("here")?.unwrap_or_default();
    assert!(!title.starts_with("i3mux-here-"), "{title}");
    Ok(())
}
