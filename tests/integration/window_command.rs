//! Window commands (README, "Window commands"; PLAN-terminal-here
//! decision 2): each window of the workspace runs the binding's command
//! instead of i3mux's attach, with the session and shell names, in every
//! window that `activate`, `terminal`, `open` and `attach` open.

use rstest::rstest;

use super::common::{terminal, wait_until, Result, Session, TestEnvironment};

const WS: u32 = 4;
const NAME: &str = "ws4";

/// The window command: it logs its two words to `~/window-commands` in the
/// Sway container, then waits. It never attaches its shell.
const WINDOW_COMMAND: &str =
    r#"sh -c 'echo "$0 $1" >> "$HOME/window-commands"; exec sleep 100000' {session} {shell}"#;

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn every_window_runs_the_window_command(#[case] session: Session) -> Result<()> {
    let env = TestEnvironment::new()?;
    let logged = || -> Result<Vec<String>> {
        Ok(env
            .host_file(Session::Local, "window-commands")?
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect())
    };
    env.focus_workspace(WS)?;
    env.i3mux()
        .run(&format!(
            "activate{} --window-command {}",
            session.remote_flag(),
            shell_words::quote(WINDOW_COMMAND)
        ))?
        .ok()?;
    wait_until("the first window ran the command", || {
        Ok(logged()? == ["ws4 ws4-001"])
    })?;
    assert_eq!(env.marks(WS)?, [session.mark(NAME, 1)]);
    let first = env.shell(session, &terminal(NAME, 1))?;
    assert!(
        first.is_some_and(|s| !s.attached),
        "i3mux made the shell, and the command did not attach it"
    );

    env.i3mux().terminal("")?;
    let made = terminal(NAME, 7);
    env.make_host_shell(session, &made, "sleep 100000")?;
    wait_until("the made shell is disconnected", || {
        Ok(env.shell(session, &made)?.is_some_and(|s| !s.attached))
    })?;
    env.i3mux()
        .run(&format!("open{} {made}", session.remote_flag()))?
        .ok()?;
    wait_until("terminal and open ran the command", || {
        Ok(logged()? == ["ws4 ws4-001", "ws4 ws4-002", "ws4 ws4-007"])
    })?;

    env.i3mux().detach()?;
    env.i3mux().attach(session, "--session ws4")?.ok()?;
    wait_until("attach ran the command in each window", || {
        Ok(logged()?.len() == 6)
    })?;
    assert_eq!(
        env.marks(WS)?,
        [
            session.mark(NAME, 1),
            session.mark(NAME, 2),
            session.mark(NAME, 7)
        ]
    );
    let status = env.i3mux().run("status")?.ok()?;
    assert!(
        status.contains(&format!("  window command: {WINDOW_COMMAND}")),
        "{status}"
    );
    Ok(())
}
