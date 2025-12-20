// Multi-way split and nested layout tests

use super::common::*;
use super::{should_ignore_session, workspace_for_session};
use rstest::rstest;
use std::time::Duration;

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_3way_hsplit(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(3, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;
    env.i3mux_activate(session.clone(), &ws)?;
    std::thread::sleep(Duration::from_millis(800)); // Wait for initial terminal from activate

    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));

    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));

    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?; // Blue
    std::thread::sleep(Duration::from_millis(800));

    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("3way-hsplit")?;
    env.compare_with_golden("3way-hsplit.png", &screenshot, &spec)?;

    println!("✓ 3-way horizontal split test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_3way_vsplit(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(4, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;
    env.i3mux_activate(session.clone(), &ws)?;
    std::thread::sleep(Duration::from_millis(800)); // Wait for initial terminal from activate

    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));

    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 45")?; // Magenta
    std::thread::sleep(Duration::from_millis(800));

    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));

    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 46")?; // Cyan
    std::thread::sleep(Duration::from_millis(800));

    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("3way-vsplit")?;
    env.compare_with_golden("3way-vsplit.png", &screenshot, &spec)?;

    println!("✓ 3-way vertical split test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_4way_grid(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(5, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;
    env.i3mux_activate(session.clone(), &ws)?;
    std::thread::sleep(Duration::from_millis(800)); // Wait for initial terminal from activate

    // Top-right (Green)
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    // Bottom-left (Blue) - focus left, split vertical
    env.i3_exec("focus left")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?; // Blue
    std::thread::sleep(Duration::from_millis(800));

    // Bottom-right (Yellow) - focus right parent, split vertical
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("focus right")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 43")?; // Yellow
    std::thread::sleep(Duration::from_millis(800));

    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("4way-grid")?;
    env.compare_with_golden("4way-grid.png", &screenshot, &spec)?;

    println!("✓ 4-way grid layout test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_nested_splits(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(6, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;
    env.i3mux_activate(session.clone(), &ws)?;
    std::thread::sleep(Duration::from_millis(800)); // Wait for initial terminal from activate

    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    // Right side - single Blue (split horizontally from the parent)
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?; // Blue
    std::thread::sleep(Duration::from_millis(800));

    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("nested-splits")?;
    env.compare_with_golden("nested-splits.png", &screenshot, &spec)?;

    println!("✓ Nested splits test passed ({:?})", session);

    Ok(())
}
