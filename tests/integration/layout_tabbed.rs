// Tabbed and stacked layout tests

use super::common::*;
use super::{should_ignore_session, workspace_for_session};
use rstest::rstest;
use std::time::Duration;

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_tabbed_2_terminals(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(20, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Launch first terminal (red)
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?;
    std::thread::sleep(Duration::from_millis(800));

    // Set layout to tabbed
    env.i3_exec("layout tabbed")?;
    std::thread::sleep(Duration::from_millis(200));

    // Launch second terminal (green)
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?;
    std::thread::sleep(Duration::from_millis(800));

    // Capture screenshots for both tabs (we're on tab 2, cycle back to tab 1 first)
    env.focus_prev_tab()?;
    std::thread::sleep(Duration::from_millis(300));

    let screenshots = env.capture_multi_screenshots(2, "next")?;

    // Compare with golden images
    let spec = ComparisonSpec::load("tabbed-2-terminals")?;
    env.compare_multi_with_golden("tabbed-2-terminals", &screenshots, &spec)?;

    println!("✓ Tabbed 2 terminals test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_stacked_2_terminals(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(21, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Launch first terminal (blue)
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?;
    std::thread::sleep(Duration::from_millis(800));

    // Set layout to stacking
    env.i3_exec("layout stacking")?;
    std::thread::sleep(Duration::from_millis(200));

    // Launch second terminal (yellow)
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 43")?;
    std::thread::sleep(Duration::from_millis(800));

    // Focus first terminal (stacked uses up/down)
    env.focus_prev_stack()?;
    std::thread::sleep(Duration::from_millis(300));

    // Capture first screenshot
    let screenshot1 = env.capture_screenshot()?;

    // Focus next stack item
    env.focus_next_stack()?;
    std::thread::sleep(Duration::from_millis(300));

    // Capture second screenshot
    let screenshot2 = env.capture_screenshot()?;

    let screenshots = vec![screenshot1, screenshot2];
    let spec = ComparisonSpec::load("stacked-2-terminals")?;
    env.compare_multi_with_golden("stacked-2-terminals", &screenshots, &spec)?;

    println!("✓ Stacked 2 terminals test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_tabbed_3_terminals(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(22, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Launch first terminal (red)
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?;
    std::thread::sleep(Duration::from_millis(800));

    // Set layout to tabbed
    env.i3_exec("layout tabbed")?;
    std::thread::sleep(Duration::from_millis(200));

    // Launch second terminal (green)
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?;
    std::thread::sleep(Duration::from_millis(800));

    // Launch third terminal (blue)
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?;
    std::thread::sleep(Duration::from_millis(800));

    // Focus first terminal (we're on tab 3)
    env.focus_prev_tab()?;
    std::thread::sleep(Duration::from_millis(200));
    env.focus_prev_tab()?;
    std::thread::sleep(Duration::from_millis(300));

    let screenshots = env.capture_multi_screenshots(3, "next")?;

    let spec = ComparisonSpec::load("tabbed-3-terminals")?;
    env.compare_multi_with_golden("tabbed-3-terminals", &screenshots, &spec)?;

    println!("✓ Tabbed 3 terminals test passed ({:?})", session);

    Ok(())
}
