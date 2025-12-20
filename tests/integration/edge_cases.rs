// Edge case tests: empty workspace, single window, many windows, focus navigation

use super::common::*;
use super::{should_ignore_session, workspace_for_session};
use rstest::rstest;
use std::time::Duration;

#[test]
fn test_empty_workspace() -> Result<()> {
    // Test that empty workspace is truly empty
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("7")?;
    env.i3_exec("workspace 7")?;

    let windows = env.get_workspace_windows()?;
    assert_eq!(windows.len(), 0, "Workspace should be empty");

    println!("✓ Empty workspace test passed");

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_single_window(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(8, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;
    env.i3mux_activate(session.clone(), &ws)?;
    std::thread::sleep(Duration::from_millis(800)); // Wait for initial terminal from activate

    let windows = env.get_workspace_windows()?;
    assert_eq!(windows.len(), 1, "Should have exactly one window");

    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("single-window")?;
    env.compare_with_golden("single-window.png", &screenshot, &spec)?;

    println!("✓ Single window test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_many_windows(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(9, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;
    env.i3mux_activate(session.clone(), &ws)?;
    std::thread::sleep(Duration::from_millis(800)); // Wait for initial terminal from activate

    let color_codes = [
        ("41", "Red"),
        ("42", "Green"),
        ("44", "Blue"),
        ("43", "Yellow"),
        ("45", "Magenta"),
        ("46", "Cyan"),
        ("41", "Red"),
    ];

    // Create 7 more windows with alternating horizontal and vertical splits (activate gave us 1)
    for (i, (code, _name)) in color_codes.iter().enumerate() {
        env.i3_exec(&format!("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh {}", code))?;
        std::thread::sleep(Duration::from_millis(800));

        if i < color_codes.len() - 1 {
            // Alternate between horizontal and vertical splits
            if i % 2 == 0 {
                env.i3_exec("split h")?;
            } else {
                env.i3_exec("split v")?;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    let windows = env.get_workspace_windows()?;
    assert_eq!(windows.len(), 8, "Should have exactly 8 windows");

    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("many-windows")?;
    env.compare_with_golden("many-windows.png", &screenshot, &spec)?;

    println!("✓ Many windows test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_focus_navigation(#[case] session: Session) -> Result<()> {
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(10, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;
    env.i3mux_activate(session.clone(), &ws)?;
    std::thread::sleep(Duration::from_millis(800)); // Wait for initial terminal from activate

    // Create horizontal split
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    // Navigate focus
    env.i3_exec("focus left")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("focus right")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("focus left")?;
    std::thread::sleep(Duration::from_millis(200));

    // Verify we can still capture screenshot after focus changes
    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("hsplit-2-terminals")?; // Same layout as hsplit test
    env.compare_with_golden("focus-navigation.png", &screenshot, &spec)?;

    println!("✓ Focus navigation test passed ({:?})", session);

    Ok(())
}

#[test]
#[ignore] // Requires remote SSH setup
fn test_workspace_cleanup_after_last_terminal_closes() -> Result<()> {
    // Test that workspace state is cleaned up when last terminal closes,
    // allowing the workspace to be reused for a different session type
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("15")?;
    env.i3_exec("workspace 15")?;

    // Activate as remote workspace
    println!("=== Activating workspace 15 as remote ===");
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "15")?;
    std::thread::sleep(Duration::from_secs(2)); // Wait for initial terminal

    // Verify we have at least one terminal
    let windows = env.get_workspace_windows()?;
    assert!(windows.len() >= 1, "Should have at least 1 terminal from activate");
    println!("Terminal count after activate: {}", windows.len());

    // Launch a second terminal to be sure we have multiple
    env.launch_i3mux_terminal()?;
    std::thread::sleep(Duration::from_millis(800));

    let windows = env.get_workspace_windows()?;
    println!("Terminal count after manual launch: {}", windows.len());

    // Close all terminals (simulating user closing all windows)
    println!("=== Closing all terminals ===");
    env.cleanup_workspace("15")?;
    std::thread::sleep(Duration::from_secs(1)); // Wait for cleanup to run

    // Verify workspace is empty
    let windows = env.get_workspace_windows()?;
    assert_eq!(windows.len(), 0, "Workspace should be empty after closing all terminals");

    // Now try to activate the same workspace as LOCAL
    // This should succeed if cleanup worked properly
    println!("=== Activating workspace 15 as local (should succeed if cleanup worked) ===");
    env.i3mux_activate(Session::Local, "15")?;
    std::thread::sleep(Duration::from_millis(800));

    // Verify we got a local terminal
    let windows = env.get_workspace_windows()?;
    assert!(windows.len() >= 1, "Should have at least 1 terminal from local activate");

    println!("✓ Workspace cleanup test passed - workspace successfully transitioned from remote to local");

    Ok(())
}
