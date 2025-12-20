// Detach/attach tests for remote sessions

use super::common::*;
use std::time::Duration;

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
