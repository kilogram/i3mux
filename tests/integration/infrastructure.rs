//! The containers are wired as the other tests assume.

use super::common::{Result, Session, TestEnvironment};

/// Sway answers, ssh reaches the remote with shpool on its login PATH, and
/// neither host has i3mux shells yet.
#[test]
fn containers_are_ready_for_local_and_remote_sessions() -> Result<()> {
    let env = TestEnvironment::new()?;
    env.focus_workspace(1)?;
    for session in [Session::Local, Session::Remote] {
        let out = env
            .i3mux()
            .run(&format!("sessions{}", session.remote_flag()))?;
        assert!(out.success(), "{out}");
        assert_eq!(out.stdout, "");
        assert!(
            out.stderr
                .contains(&format!("No sessions on {}", session.host_label())),
            "{out}"
        );
        assert!(env.shells(session)?.is_empty());
    }
    Ok(())
}
