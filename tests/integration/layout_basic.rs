// Basic layout tests: horizontal and vertical splits with 2 terminals

use super::common::*;
use super::{should_ignore_session, workspace_for_session};
use rstest::rstest;
use std::time::Duration;

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_hsplit_2_terminals(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(1, &session);

    // Clean up and activate workspace
    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;
    env.i3mux_activate(session.clone(), &ws)?;
    std::thread::sleep(Duration::from_millis(800)); // Wait for initial terminal from activate

    // Split and launch one more terminal (activate gave us the first one)
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));

    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    // Capture screenshot
    let screenshot = env.capture_screenshot()?;

    // Compare with golden image (same golden for both local and remote)
    let spec = ComparisonSpec::load("hsplit-2-terminals")?;
    env.compare_with_golden("hsplit-2-terminals.png", &screenshot, &spec)?;

    println!("✓ Horizontal split 2 terminals test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_vsplit_2_terminals(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(2, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;
    env.i3mux_activate(session.clone(), &ws)?;
    std::thread::sleep(Duration::from_millis(800)); // Wait for initial terminal from activate

    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));

    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 43")?; // Yellow
    std::thread::sleep(Duration::from_millis(800));

    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("vsplit-2-terminals")?;
    env.compare_with_golden("vsplit-2-terminals.png", &screenshot, &spec)?;

    println!("✓ Vertical split 2 terminals test passed ({:?})", session);

    Ok(())
}
