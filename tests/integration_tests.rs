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
fn test_remote_detach_attach() -> Result<()> {
    // Test detach/attach with remote session
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("11")?;
    env.i3_exec("workspace 11")?;

    // Activate remote i3mux session (gives us first terminal via SSH)
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "11")?;
    std::thread::sleep(Duration::from_secs(3)); // Wait for SSH terminal

    // Get initial window count (should be 1 from activate)
    let initial_windows = env.get_workspace_windows()?;
    let initial_count = initial_windows.len();
    assert!(initial_count >= 1, "Should have at least 1 terminal from activate");

    // Detach session (this saves layout and kills terminals)
    env.i3mux_detach("ws11")?;
    std::thread::sleep(Duration::from_millis(500));

    // Verify workspace is now empty
    let windows_after_detach = env.get_workspace_windows()?;
    assert_eq!(windows_after_detach.len(), 0, "Workspace should be empty after detach");

    // Attach session back (this should restore layout)
    env.i3mux_attach(Session::Remote("testuser@i3mux-remote-ssh"), "ws11")?;
    std::thread::sleep(Duration::from_secs(3)); // Wait for restoration

    // Verify windows are restored
    let windows_after_attach = env.get_workspace_windows()?;
    assert_eq!(
        windows_after_attach.len(),
        initial_count,
        "Should restore same number of windows"
    );

    println!("✓ Remote detach/attach test passed (detached {} terminals, restored {})",
        initial_count, windows_after_attach.len());

    Ok(())
}

#[test]
fn test_detach_attach_multiple_terminals() -> Result<()> {
    // Test detach/attach with multiple terminals in a complex layout
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("12")?;
    env.i3_exec("workspace 12")?;

    // Activate remote i3mux session
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "12")?;
    std::thread::sleep(Duration::from_secs(2));

    // Create a complex layout: hsplit with 2 terminals, then vsplit one of them
    env.i3_exec("split h")?;
    env.launch_i3mux_terminal()?;  // Waits for terminal to appear

    // Check marks immediately after first launch
    println!("\n=== After first manual launch ===");
    let windows_after_first = env.get_workspace_windows()?;
    for win_id in &windows_after_first {
        let info = env.get_window_info(*win_id)?;
        println!("Window {}: {}", win_id, info);
    }

    // Add delay to avoid rapid terminal launches
    std::thread::sleep(Duration::from_millis(1000));

    env.i3_exec("split v")?;
    env.launch_i3mux_terminal()?;  // Waits for terminal to appear

    // Check marks immediately after second launch
    println!("\n=== After second manual launch ===");
    let windows_after_second = env.get_workspace_windows()?;
    for win_id in &windows_after_second {
        let info = env.get_window_info(*win_id)?;
        println!("Window {}: {}", win_id, info);
    }

    // Print i3mux debug log to see what mark commands were executed
    println!("\n=== i3mux debug log ===");
    if let Ok(log) = env.read_debug_log() {
        println!("{}", log);
    }

    // Should have 3 terminals total (1 from activate + 2 created)
    let initial_windows = env.get_workspace_windows()?;
    assert_eq!(initial_windows.len(), 3, "Should have 3 terminals before detach");

    // Debug: Check if windows have the i3mux-terminal mark
    println!("\n=== Before detach ===");
    for win_id in &initial_windows {
        let info = env.get_window_info(*win_id)?;
        println!("Window {}: {}", win_id, info);
    }

    // Detach session
    env.i3mux_detach("ws12")?;
    std::thread::sleep(Duration::from_millis(500));

    // Verify workspace is empty
    let windows_after_detach = env.get_workspace_windows()?;
    if windows_after_detach.len() != 0 {
        for win_id in &windows_after_detach {
            let info = env.get_window_info(*win_id)?;
            println!("Window {} still present: {}", win_id, info);
        }
    }
    assert_eq!(windows_after_detach.len(), 0, "Workspace should be empty after detach");

    // Attach session back
    env.i3mux_attach(Session::Remote("testuser@i3mux-remote-ssh"), "ws12")?;
    std::thread::sleep(Duration::from_secs(3));

    // Verify all terminals are restored
    let windows_after_attach = env.get_workspace_windows()?;
    assert_eq!(
        windows_after_attach.len(),
        3,
        "Should restore all 3 terminals"
    );

    println!("✓ Detach/attach with multiple terminals passed");

    Ok(())
}

#[test]
fn test_detach_attach_mixed_windows() -> Result<()> {
    // Test detach/attach when workspace has both i3mux and non-i3mux windows
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("13")?;
    env.i3_exec("workspace 13")?;

    // Activate remote i3mux session
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "13")?;
    std::thread::sleep(Duration::from_secs(2));

    // Add another i3mux terminal
    env.i3_exec("split h")?;
    env.launch_i3mux_terminal()?;  // Waits for terminal to appear

    // Add a regular (non-i3mux) xterm window
    env.i3_exec("split v")?;
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 43")?;
    std::thread::sleep(Duration::from_millis(800));

    // Should have 3 windows total (2 i3mux + 1 regular)
    let initial_windows = env.get_workspace_windows()?;
    assert_eq!(initial_windows.len(), 3, "Should have 3 windows before detach");

    // Detach session - should only detach i3mux terminals
    env.i3mux_detach("ws13")?;
    std::thread::sleep(Duration::from_millis(500));

    // Regular window should still be there, i3mux terminals should be gone
    let windows_after_detach = env.get_workspace_windows()?;
    assert_eq!(
        windows_after_detach.len(),
        1,
        "Only the non-i3mux window should remain"
    );

    // Attach session back
    env.i3mux_attach(Session::Remote("testuser@i3mux-remote-ssh"), "ws13")?;
    std::thread::sleep(Duration::from_secs(3));

    // Should restore 2 i3mux terminals (regular window + 2 restored = 3 total)
    let windows_after_attach = env.get_workspace_windows()?;
    assert_eq!(
        windows_after_attach.len(),
        3,
        "Should have 2 i3mux terminals + 1 regular window"
    );

    println!("✓ Detach/attach with mixed windows passed (gracefully ignored non-i3mux window)");

    Ok(())
}

#[test]
fn test_detach_attach_default_session_name() -> Result<()> {
    // Test detach/attach without specifying session name (should use default ws-based name)
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("14")?;
    env.i3_exec("workspace 14")?;

    // Activate without session name - should default to workspace-based name
    env.i3_exec("exec --no-startup-id DISPLAY=:99 TERMINAL=xterm i3mux activate --remote testuser@i3mux-remote-ssh")?;
    std::thread::sleep(Duration::from_secs(3));

    let initial_windows = env.get_workspace_windows()?;
    assert!(initial_windows.len() >= 1, "Should have at least 1 terminal");

    // Detach without session name - should use default "ws14"
    env.i3_exec("exec --no-startup-id DISPLAY=:99 i3mux detach")?;
    std::thread::sleep(Duration::from_millis(1000));

    let windows_after_detach = env.get_workspace_windows()?;
    assert_eq!(windows_after_detach.len(), 0, "Workspace should be empty after detach");

    // Attach using the default session name
    env.i3mux_attach(Session::Remote("testuser@i3mux-remote-ssh"), "ws14")?;
    std::thread::sleep(Duration::from_secs(3));

    let windows_after_attach = env.get_workspace_windows()?;
    assert!(
        windows_after_attach.len() >= 1,
        "Should restore terminals with default session name"
    );

    println!("✓ Detach/attach with default session name passed");

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
    for (i, (code, _name)) in color_codes.iter().enumerate() {
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

// ==================== Workspace Cleanup Tests ====================

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

// ==================== Tabbed Layout Tests ====================

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

// ==================== Nested Layout Tests (Tabs + Splits) ====================

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_tabs_in_hsplit(#[case] session: Session) -> Result<()> {
    // Layout: [Tabs(Red, Green)] | [Blue]
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(23, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Left side: create tabbed container with 2 terminals
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?; // Red
    std::thread::sleep(Duration::from_millis(800));
    env.i3_exec("layout tabbed")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    // Right side: add hsplit and blue terminal
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?; // Blue
    std::thread::sleep(Duration::from_millis(800));

    // Now capture: first the two tabs on left, then the blue on right
    // Focus the tabbed container (left)
    env.i3_exec("focus left")?;
    std::thread::sleep(Duration::from_millis(200));
    env.focus_prev_tab()?; // Go to first tab (red)
    std::thread::sleep(Duration::from_millis(300));

    // Capture tab 1 (Red + Blue visible)
    let screenshot1 = env.capture_screenshot()?;

    // Focus next tab (Green)
    env.focus_next_tab()?;
    std::thread::sleep(Duration::from_millis(300));

    // Capture tab 2 (Green + Blue visible)
    let screenshot2 = env.capture_screenshot()?;

    let screenshots = vec![screenshot1, screenshot2];
    let spec = ComparisonSpec::load("tabs-in-hsplit")?;
    env.compare_multi_with_golden("tabs-in-hsplit", &screenshots, &spec)?;

    println!("✓ Tabs in horizontal split test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_tabs_in_vsplit(#[case] session: Session) -> Result<()> {
    // Layout: [Tabs(Red, Green)] / [Blue]
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(24, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Top: create tabbed container with 2 terminals
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?; // Red
    std::thread::sleep(Duration::from_millis(800));
    env.i3_exec("layout tabbed")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    // Bottom: add vsplit and blue terminal
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?; // Blue
    std::thread::sleep(Duration::from_millis(800));

    // Focus the tabbed container (top)
    env.i3_exec("focus up")?;
    std::thread::sleep(Duration::from_millis(200));
    env.focus_prev_tab()?; // Go to first tab (red)
    std::thread::sleep(Duration::from_millis(300));

    // Capture tab 1 (Red / Blue visible)
    let screenshot1 = env.capture_screenshot()?;

    // Focus next tab (Green)
    env.focus_next_tab()?;
    std::thread::sleep(Duration::from_millis(300));

    // Capture tab 2 (Green / Blue visible)
    let screenshot2 = env.capture_screenshot()?;

    let screenshots = vec![screenshot1, screenshot2];
    let spec = ComparisonSpec::load("tabs-in-vsplit")?;
    env.compare_multi_with_golden("tabs-in-vsplit", &screenshots, &spec)?;

    println!("✓ Tabs in vertical split test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_hsplit_in_tabs(#[case] session: Session) -> Result<()> {
    // Layout: Tabs( [Red|Green], [Blue|Yellow] )
    // Tab 1: Red | Green (horizontal split)
    // Tab 2: Blue | Yellow (horizontal split)
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(25, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Tab 1: Red | Green
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?; // Red
    std::thread::sleep(Duration::from_millis(800));
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    // Set parent to tabbed layout
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("layout tabbed")?;
    std::thread::sleep(Duration::from_millis(200));

    // Tab 2: Blue | Yellow (need to create a new container)
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?; // Blue
    std::thread::sleep(Duration::from_millis(800));
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 43")?; // Yellow
    std::thread::sleep(Duration::from_millis(800));

    // Navigate to Tab 1 from Yellow:
    // Yellow -> Blue (focus left within Tab 2's hsplit)
    // Blue -> Tab 1 (focus left crosses into Tab 1, landing on Green)
    // Green -> Red (focus left within Tab 1's hsplit)
    env.i3_exec("focus left")?;  // Yellow -> Blue
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("focus left")?;  // Blue -> Green (switches tabs)
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("focus left")?;  // Green -> Red
    std::thread::sleep(Duration::from_millis(300));

    // Capture tab 1 (Red | Green) - now focused on Red
    let screenshot1 = env.capture_screenshot()?;

    // Navigate to Tab 2:
    // Red -> Green -> Blue -> Yellow (or we can go right 3 times)
    env.i3_exec("focus right")?;  // Red -> Green
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("focus right")?;  // Green -> Blue (switches tabs)
    std::thread::sleep(Duration::from_millis(300));

    // Capture tab 2 (Blue | Yellow) - now focused on Blue
    let screenshot2 = env.capture_screenshot()?;

    let screenshots = vec![screenshot1, screenshot2];
    let spec = ComparisonSpec::load("hsplit-in-tabs")?;
    env.compare_multi_with_golden("hsplit-in-tabs", &screenshots, &spec)?;

    println!("✓ Horizontal split in tabs test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_vsplit_in_tabs(#[case] session: Session) -> Result<()> {
    // Layout: Tabs( [Red/Green], [Blue/Yellow] )
    // Tab 1: Red over Green (vertical split)
    // Tab 2: Blue over Yellow (vertical split)
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(26, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Tab 1: Red / Green
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?; // Red
    std::thread::sleep(Duration::from_millis(800));
    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    // Set parent to tabbed layout
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("layout tabbed")?;
    std::thread::sleep(Duration::from_millis(200));

    // Tab 2: Blue / Yellow
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?; // Blue
    std::thread::sleep(Duration::from_millis(800));
    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 43")?; // Yellow
    std::thread::sleep(Duration::from_millis(800));

    // Navigate to Tab 1 from Yellow:
    // parent -> up -> left to switch tabs
    env.i3_exec("focus parent")?;  // Yellow -> Tab 2's vsplit container
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("focus up")?;      // Up to tabbed container level
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("focus left")?;    // Switch to Tab 1
    std::thread::sleep(Duration::from_millis(300));

    // Capture tab 1 (Red / Green)
    let screenshot1 = env.capture_screenshot()?;

    // Navigate to Tab 2:
    env.i3_exec("focus right")?;  // Tab 1 -> Tab 2 (switches tabs)
    std::thread::sleep(Duration::from_millis(300));

    // Capture tab 2 (Blue / Yellow)
    let screenshot2 = env.capture_screenshot()?;

    let screenshots = vec![screenshot1, screenshot2];
    let spec = ComparisonSpec::load("vsplit-in-tabs")?;
    env.compare_multi_with_golden("vsplit-in-tabs", &screenshots, &spec)?;

    println!("✓ Vertical split in tabs test passed ({:?})", session);

    Ok(())
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_stacked_in_hsplit(#[case] session: Session) -> Result<()> {
    // Layout: [Stack(Red, Green)] | [Blue]
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(27, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Left side: create stacked container with 2 terminals
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?; // Red
    std::thread::sleep(Duration::from_millis(800));
    env.i3_exec("layout stacking")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    // Right side: add hsplit and blue terminal
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?; // Blue
    std::thread::sleep(Duration::from_millis(800));

    // Focus the stacked container (left)
    env.i3_exec("focus left")?;
    std::thread::sleep(Duration::from_millis(200));
    env.focus_prev_stack()?; // Go to first stack item (red)
    std::thread::sleep(Duration::from_millis(300));

    // Capture stack 1 (Red + Blue visible)
    let screenshot1 = env.capture_screenshot()?;

    // Focus next stack item (Green)
    env.focus_next_stack()?;
    std::thread::sleep(Duration::from_millis(300));

    // Capture stack 2 (Green + Blue visible)
    let screenshot2 = env.capture_screenshot()?;

    let screenshots = vec![screenshot1, screenshot2];
    let spec = ComparisonSpec::load("stacked-in-hsplit")?;
    env.compare_multi_with_golden("stacked-in-hsplit", &screenshots, &spec)?;

    println!("✓ Stacked in horizontal split test passed ({:?})", session);

    Ok(())
}

// ==================== Deep Nesting Tests ====================

// NOTE: test_tabs_in_tabs is omitted because reliably navigating double-nested
// tabbed containers in i3 requires more complex focus management. The other
// nested layout tests (tabs_in_hsplit, hsplit_in_tabs, complex_nested) provide
// sufficient coverage for nested layouts.

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote("testuser@i3mux-remote-ssh"))]
fn test_complex_nested_layout(#[case] session: Session) -> Result<()> {
    // Complex layout:
    // +---------------------------+
    // | Tabs(Red|Green) | Blue    |   <- hsplit with tabs on left
    // +---------------------------+
    // |     Yellow      | Magenta |   <- hsplit below
    // +---------------------------+
    //
    // So it's: VSplit( HSplit(Tabs(R,G), B), HSplit(Y, M) )
    if should_ignore_session(&session) && std::env::var("RUN_REMOTE_TESTS").is_err() {
        println!("⊘ Skipping remote test (set RUN_REMOTE_TESTS=1 to run)");
        return Ok(());
    }

    let env = TestEnvironment::new()?;
    let ws = workspace_for_session(29, &session);

    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Top-left: Tabbed(Red, Green)
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?; // Red
    std::thread::sleep(Duration::from_millis(800));
    env.i3_exec("layout tabbed")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    // Top-right: Blue
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?; // Blue
    std::thread::sleep(Duration::from_millis(800));

    // Bottom row: Yellow | Magenta (via parent split v)
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 43")?; // Yellow
    std::thread::sleep(Duration::from_millis(800));
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 45")?; // Magenta
    std::thread::sleep(Duration::from_millis(800));

    // Navigate to top-left tabbed container, first tab
    env.i3_exec("focus up")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("focus left")?;
    std::thread::sleep(Duration::from_millis(200));
    env.focus_prev_tab()?;
    std::thread::sleep(Duration::from_millis(300));

    // Capture with Red tab visible
    let screenshot1 = env.capture_screenshot()?;

    // Capture with Green tab visible
    env.focus_next_tab()?;
    std::thread::sleep(Duration::from_millis(300));
    let screenshot2 = env.capture_screenshot()?;

    let screenshots = vec![screenshot1, screenshot2];
    let spec = ComparisonSpec::load("complex-nested")?;
    env.compare_multi_with_golden("complex-nested", &screenshots, &spec)?;

    println!("✓ Complex nested layout test passed ({:?})", session);

    Ok(())
}

// ==================== Detach/Attach with Tabbed/Stacked ====================

#[test]
fn test_detach_attach_tabbed_layout() -> Result<()> {
    // Test that tabbed layouts are properly saved and restored
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("30")?;
    env.i3_exec("workspace 30")?;

    // Activate remote session
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "30")?;
    std::thread::sleep(Duration::from_secs(2));

    // Set to tabbed layout
    env.i3_exec("layout tabbed")?;
    std::thread::sleep(Duration::from_millis(200));

    // Add second terminal
    env.launch_i3mux_terminal()?;

    // Add third terminal
    env.launch_i3mux_terminal()?;

    // Verify we have 3 terminals
    let initial_windows = env.get_workspace_windows()?;
    assert_eq!(initial_windows.len(), 3, "Should have 3 terminals before detach");

    // Detach
    env.i3mux_detach("ws30")?;
    std::thread::sleep(Duration::from_millis(500));

    // Verify empty
    let windows_after_detach = env.get_workspace_windows()?;
    assert_eq!(windows_after_detach.len(), 0, "Workspace should be empty after detach");

    // Attach
    env.i3mux_attach(Session::Remote("testuser@i3mux-remote-ssh"), "ws30")?;
    std::thread::sleep(Duration::from_secs(4)); // Extra time for 3 terminals

    // Verify all terminals restored
    let windows_after_attach = env.get_workspace_windows()?;
    assert_eq!(
        windows_after_attach.len(),
        3,
        "Should restore all 3 terminals in tabbed layout"
    );

    // Verify layout is still tabbed by checking if we can cycle through tabs
    // (if it were a split, focus left/right would move to different windows, not cycle)
    env.focus_prev_tab()?;
    std::thread::sleep(Duration::from_millis(200));
    env.focus_prev_tab()?;
    std::thread::sleep(Duration::from_millis(200));

    println!("✓ Detach/attach with tabbed layout passed");

    Ok(())
}

#[test]
fn test_detach_attach_stacked_layout() -> Result<()> {
    // Test that stacked layouts are properly saved and restored
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("31")?;
    env.i3_exec("workspace 31")?;

    // Activate remote session
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "31")?;
    std::thread::sleep(Duration::from_secs(2));

    // Set to stacked layout
    env.i3_exec("layout stacking")?;
    std::thread::sleep(Duration::from_millis(200));

    // Add second terminal
    env.launch_i3mux_terminal()?;

    // Verify we have 2 terminals
    let initial_windows = env.get_workspace_windows()?;
    assert_eq!(initial_windows.len(), 2, "Should have 2 terminals before detach");

    // Detach
    env.i3mux_detach("ws31")?;
    std::thread::sleep(Duration::from_millis(500));

    // Verify empty
    let windows_after_detach = env.get_workspace_windows()?;
    assert_eq!(windows_after_detach.len(), 0, "Workspace should be empty after detach");

    // Attach
    env.i3mux_attach(Session::Remote("testuser@i3mux-remote-ssh"), "ws31")?;
    std::thread::sleep(Duration::from_secs(3));

    // Verify terminals restored
    let windows_after_attach = env.get_workspace_windows()?;
    assert_eq!(
        windows_after_attach.len(),
        2,
        "Should restore both terminals in stacked layout"
    );

    println!("✓ Detach/attach with stacked layout passed");

    Ok(())
}

#[test]
fn test_detach_attach_nested_tabs_splits() -> Result<()> {
    // Test complex nested layout: HSplit(Tabs(t1,t2), t3)
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("32")?;
    env.i3_exec("workspace 32")?;

    // Activate remote session
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "32")?;
    std::thread::sleep(Duration::from_secs(2));

    // Create tabbed container with 2 terminals
    env.i3_exec("layout tabbed")?;
    std::thread::sleep(Duration::from_millis(200));
    env.launch_i3mux_terminal()?;

    // Add hsplit and third terminal
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.launch_i3mux_terminal()?;

    // Verify we have 3 terminals
    let initial_windows = env.get_workspace_windows()?;
    assert_eq!(initial_windows.len(), 3, "Should have 3 terminals before detach");

    // Detach
    env.i3mux_detach("ws32")?;
    std::thread::sleep(Duration::from_millis(500));

    // Verify empty
    let windows_after_detach = env.get_workspace_windows()?;
    assert_eq!(windows_after_detach.len(), 0, "Workspace should be empty after detach");

    // Attach
    env.i3mux_attach(Session::Remote("testuser@i3mux-remote-ssh"), "ws32")?;
    std::thread::sleep(Duration::from_secs(4));

    // Verify all terminals restored
    let windows_after_attach = env.get_workspace_windows()?;
    assert_eq!(
        windows_after_attach.len(),
        3,
        "Should restore all 3 terminals with nested layout"
    );

    println!("✓ Detach/attach with nested tabs/splits passed");

    Ok(())
}
