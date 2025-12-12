// Integration tests for i3mux
// Run with: cargo test --test integration_tests
// Run including remote tests: cargo test --test integration_tests -- --include-ignored
// Update goldens with: UPDATE_GOLDENS=1 cargo test --test integration_tests

mod common;

use common::*;
use rstest::rstest;
use std::time::Duration;

// ==================== Parameterized Session Types ====================
// Tests run for both local and remote sessions (remote tests are #[ignore] by default)

/// Helper to get workspace number for a session type
fn workspace_for_session(base: u32, session: &Session) -> String {
    match session {
        Session::Local => base.to_string(),
        Session::Remote(_) => (base + 100).to_string(), // Offset remote tests to avoid conflicts
    }
}

/// Helper to determine if test should be ignored (for remote sessions)
fn should_ignore_session(session: &Session) -> bool {
    matches!(session, Session::Remote(_))
}

// ==================== Layout Restoration Tests ====================
// NOTE: Detach/attach only works for remote sessions (not local)

#[test]
#[ignore] // Requires SSH setup
fn test_remote_detach_attach_hsplit() -> Result<()> {
    // Test detach/attach with layout preservation for remote session
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("11")?;
    env.i3_exec("workspace 11")?;

    // Activate remote i3mux session
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "11")?;

    // Create 2-terminal horizontal split layout
    let _term1 = env.launch_terminal(ColorScript::Red)?;
    env.wait_for_ssh_connection(_term1, Duration::from_secs(3))?;

    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));

    let _term2 = env.launch_terminal(ColorScript::Green)?;
    env.wait_for_ssh_connection(_term2, Duration::from_secs(3))?;

    // Capture "before" screenshot
    let before_screenshot = env.capture_screenshot()?;

    // Detach session (this saves layout and kills terminals)
    env.i3mux_detach("ws11")?;
    std::thread::sleep(Duration::from_millis(500));

    // Verify workspace is now empty
    let windows = env.get_workspace_windows()?;
    assert_eq!(windows.len(), 0, "Workspace should be empty after detach");

    // Attach session back (this should restore layout)
    env.i3mux_attach(Session::Remote("testuser@i3mux-remote-ssh"), "ws11")?;
    env.wait_for_layout_restore(Duration::from_secs(3))?;

    // Capture "after" screenshot
    let after_screenshot = env.capture_screenshot()?;

    // Compare before and after - layout should be restored
    let spec = ComparisonSpec::load("hsplit-2-terminals")?;
    env.compare_with_golden("detach-attach-hsplit.png", &before_screenshot, &spec)?;
    env.compare_with_golden("detach-attach-hsplit.png", &after_screenshot, &spec)?;

    println!("✓ Remote detach/attach test passed");

    Ok(())
}

#[test]
#[ignore] // Requires SSH setup
fn test_remote_detach_attach_complex() -> Result<()> {
    // Test detach/attach with complex nested layout
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("12")?;
    env.i3_exec("workspace 12")?;

    // Activate remote session
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "12")?;

    // Create nested layout: left vertical split (Red/Green), right single (Blue)
    let _term1 = env.launch_terminal(ColorScript::Red)?;
    env.wait_for_ssh_connection(_term1, Duration::from_secs(3))?;

    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));

    let _term2 = env.launch_terminal(ColorScript::Green)?;
    env.wait_for_ssh_connection(_term2, Duration::from_secs(3))?;

    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));

    let _term3 = env.launch_terminal(ColorScript::Blue)?;
    env.wait_for_ssh_connection(_term3, Duration::from_secs(3))?;

    // Capture before detach
    let before_screenshot = env.capture_screenshot()?;

    // Detach
    env.i3mux_detach("ws12")?;
    std::thread::sleep(Duration::from_millis(500));

    // Verify empty
    let windows = env.get_workspace_windows()?;
    assert_eq!(windows.len(), 0, "Workspace should be empty after detach");

    // Attach back
    env.i3mux_attach(Session::Remote("testuser@i3mux-remote-ssh"), "ws12")?;
    env.wait_for_layout_restore(Duration::from_secs(4))?;

    // Capture after attach
    let after_screenshot = env.capture_screenshot()?;

    // Compare
    let spec = ComparisonSpec::load("nested-splits")?;
    env.compare_with_golden("detach-attach-complex.png", &before_screenshot, &spec)?;
    env.compare_with_golden("detach-attach-complex.png", &after_screenshot, &spec)?;

    println!("✓ Remote detach/attach complex layout test passed");

    Ok(())
}

// ==================== Remote Session Tests ====================
// NOTE: Remote session tests are handled via parameterized tests above
// (each layout test runs for both Session::Local and Session::Remote)

// ==================== Network Failure Tests ====================

#[test]
#[ignore] // Ignore by default as it requires SSH setup and network manipulation
fn test_remote_with_latency() -> Result<()> {
    let env = TestEnvironment::new()?;

    // Inject 200ms latency with 50ms jitter
    env.inject_latency(200, 50)?;

    // Clean up workspace
    env.cleanup_workspace("5")?;

    // Activate remote session (should still work, just slower)
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "5")?;

    // Launch terminal
    let _term = env.launch_terminal(ColorScript::Blue)?;
    env.wait_for_ssh_connection(_term, Duration::from_secs(5))?;

    // Clear network rules
    env.clear_network_rules()?;

    println!("✓ Remote session with latency test passed");

    Ok(())
}

#[test]
#[ignore] // Ignore by default
fn test_remote_with_packet_loss() -> Result<()> {
    let env = TestEnvironment::new()?;

    // Inject 20% packet loss
    env.inject_packet_loss(20)?;

    // Clean up workspace
    env.cleanup_workspace("6")?;

    // Activate remote session
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "6")?;

    // Launch terminal (should still work with retries)
    let _term = env.launch_terminal(ColorScript::Magenta)?;
    env.wait_for_ssh_connection(_term, Duration::from_secs(10))?;

    // Clear network rules
    env.clear_network_rules()?;

    println!("✓ Remote session with packet loss test passed");

    Ok(())
}

// ==================== Helper to run ignored tests ====================

#[test]
fn test_basic_infrastructure() -> Result<()> {
    // This test just verifies the basic infrastructure is working
    let env = TestEnvironment::new()?;

    // Verify we can execute i3 commands
    env.i3_exec("workspace 1")?;

    // Verify we can check workspace state
    let empty = env.is_workspace_empty("999")?;
    assert!(empty);

    println!("✓ Basic infrastructure test passed");

    Ok(())
}

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

// ==================== Multi-way Split Tests ====================

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

// ==================== Nested Layout Tests ====================

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

// ==================== Edge Case Tests ====================

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
    for (i, (code, name)) in color_codes.iter().enumerate() {
        env.i3_exec(&format!("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh {}", code))?; // {name}
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

// ==================== Window Focus Tests ====================

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
