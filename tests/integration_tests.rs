// Integration tests for i3mux
// Run with: cargo test --test integration_tests
// Update goldens with: UPDATE_GOLDENS=1 cargo test --test integration_tests

mod common;

use common::*;
use std::time::Duration;

// ==================== Local Session Tests ====================
// NOTE: These tests are commented out because they require i3mux activate/detach/attach
// commands which are not yet implemented. Uncomment when those features are ready.

// #[test]
// fn test_local_hsplit_2_terminals() -> Result<()> {
//     // TODO: Requires `i3mux activate` command
//     Ok(())
// }
//
// #[test]
// fn test_local_vsplit_2_terminals() -> Result<()> {
//     // TODO: Requires `i3mux activate` command
//     Ok(())
// }

// ==================== Layout Restoration Tests ====================
// These will test detach/attach functionality once implemented

// #[test]
// fn test_local_detach_attach_hsplit() -> Result<()> {
//     // TODO: Test detach/attach with layout preservation
//     Ok(())
// }
//
// #[test]
// fn test_remote_detach_attach() -> Result<()> {
//     // TODO: Test detach from one host, attach from another
//     Ok(())
// }
//
// #[test]
// fn test_resolution_change() -> Result<()> {
//     // TODO: Test detach at one resolution, attach at different resolution
//     // Verify layout adapts correctly
//     Ok(())
// }

// ==================== Remote Session Tests ====================

#[test]
#[ignore] // Ignore by default as it requires SSH setup
fn test_remote_hsplit_2_terminals() -> Result<()> {
    let env = TestEnvironment::new()?;

    // Clean up any existing state
    env.cleanup_workspace("4")?;

    // Activate i3mux for workspace 4 (remote)
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "4")?;

    // Launch first terminal (red) via SSH
    let _term1 = env.launch_terminal(ColorScript::Red)?;
    env.wait_for_ssh_connection(_term1, Duration::from_secs(3))?;

    // Split horizontally
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));

    // Launch second terminal (green) via SSH
    let _term2 = env.launch_terminal(ColorScript::Green)?;
    env.wait_for_ssh_connection(_term2, Duration::from_secs(3))?;

    // Capture screenshot
    let screenshot = env.capture_screenshot()?;

    // Compare with golden image
    let spec = ComparisonSpec::load("hsplit-2-terminals")?;
    env.compare_with_golden("remote/hsplit-2-terminals.png", &screenshot, &spec)?;

    println!("✓ Remote horizontal split test passed");

    Ok(())
}

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

#[test]
fn test_simple_colored_layout() -> Result<()> {
    // Simple test to generate golden images without needing i3mux activate
    let env = TestEnvironment::new()?;

    // Clean up workspace
    env.cleanup_workspace("1")?;
    env.i3_exec("workspace 1")?;

    // Launch two colored terminals side by side
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?; // Red
    std::thread::sleep(Duration::from_millis(800));

    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));

    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?; // Green
    std::thread::sleep(Duration::from_millis(800));

    // Capture screenshot
    let screenshot = env.capture_screenshot()?;

    // Compare with golden image
    let spec = ComparisonSpec::load("hsplit-2-terminals")?;
    env.compare_with_golden("local/hsplit-2-terminals.png", &screenshot, &spec)?;

    println!("✓ Simple colored layout test passed");

    Ok(())
}

#[test]
fn test_simple_vsplit_layout() -> Result<()> {
    // Test vertical split layout
    let env = TestEnvironment::new()?;

    // Clean up workspace
    env.cleanup_workspace("2")?;
    env.i3_exec("workspace 2")?;

    // Launch two colored terminals stacked vertically
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?; // Blue
    std::thread::sleep(Duration::from_millis(800));

    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));

    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 43")?; // Yellow
    std::thread::sleep(Duration::from_millis(800));

    // Capture screenshot
    let screenshot = env.capture_screenshot()?;

    // Compare with golden image
    let spec = ComparisonSpec::load("vsplit-2-terminals")?;
    env.compare_with_golden("local/vsplit-2-terminals.png", &screenshot, &spec)?;

    println!("✓ Simple vsplit layout test passed");

    Ok(())
}

// ==================== Multi-way Split Tests ====================

#[test]
fn test_3way_hsplit() -> Result<()> {
    // Test 3-way horizontal split
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("3")?;
    env.i3_exec("workspace 3")?;

    // Launch three terminals side by side
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?; // Red
    std::thread::sleep(Duration::from_millis(800));

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
    env.compare_with_golden("local/3way-hsplit.png", &screenshot, &spec)?;

    println!("✓ 3-way horizontal split test passed");

    Ok(())
}

#[test]
fn test_3way_vsplit() -> Result<()> {
    // Test 3-way vertical split
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("4")?;
    env.i3_exec("workspace 4")?;

    // Launch three terminals stacked vertically
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 43")?; // Yellow
    std::thread::sleep(Duration::from_millis(800));

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
    env.compare_with_golden("local/3way-vsplit.png", &screenshot, &spec)?;

    println!("✓ 3-way vertical split test passed");

    Ok(())
}

#[test]
fn test_4way_grid() -> Result<()> {
    // Test 2x2 grid layout
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("5")?;
    env.i3_exec("workspace 5")?;

    // Top-left (Red)
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?;
    std::thread::sleep(Duration::from_millis(800));

    // Top-right (Green)
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?;
    std::thread::sleep(Duration::from_millis(800));

    // Bottom-left (Blue) - focus left, split vertical
    env.i3_exec("focus left")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?;
    std::thread::sleep(Duration::from_millis(800));

    // Bottom-right (Yellow) - focus right parent, split vertical
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("focus right")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 43")?;
    std::thread::sleep(Duration::from_millis(800));

    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("4way-grid")?;
    env.compare_with_golden("local/4way-grid.png", &screenshot, &spec)?;

    println!("✓ 4-way grid layout test passed");

    Ok(())
}

// ==================== Nested Layout Tests ====================

#[test]
fn test_nested_splits() -> Result<()> {
    // Test nested horizontal and vertical splits
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("6")?;
    env.i3_exec("workspace 6")?;

    // Left side - vertical split of Red and Green
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?;
    std::thread::sleep(Duration::from_millis(800));

    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?;
    std::thread::sleep(Duration::from_millis(800));

    // Right side - single Blue (split horizontally from the parent)
    env.i3_exec("focus parent")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 44")?;
    std::thread::sleep(Duration::from_millis(800));

    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("nested-splits")?;
    env.compare_with_golden("local/nested-splits.png", &screenshot, &spec)?;

    println!("✓ Nested splits test passed");

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

#[test]
fn test_single_window() -> Result<()> {
    // Test single window layout
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("8")?;
    env.i3_exec("workspace 8")?;

    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 45")?; // Magenta
    std::thread::sleep(Duration::from_millis(800));

    let windows = env.get_workspace_windows()?;
    assert_eq!(windows.len(), 1, "Should have exactly one window");

    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("single-window")?;
    env.compare_with_golden("local/single-window.png", &screenshot, &spec)?;

    println!("✓ Single window test passed");

    Ok(())
}

#[test]
fn test_many_windows() -> Result<()> {
    // Stress test with 8 windows in various splits
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("9")?;
    env.i3_exec("workspace 9")?;

    let colors = [41, 42, 44, 43, 45, 46, 41, 42]; // Red, Green, Blue, Yellow, Magenta, Cyan, Red, Green

    // Create 8 windows with alternating horizontal and vertical splits
    for (i, color) in colors.iter().enumerate() {
        env.i3_exec(&format!("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh {}", color))?;
        std::thread::sleep(Duration::from_millis(800));

        if i < colors.len() - 1 {
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
    env.compare_with_golden("local/many-windows.png", &screenshot, &spec)?;

    println!("✓ Many windows test passed");

    Ok(())
}

// ==================== Window Focus Tests ====================

#[test]
fn test_focus_navigation() -> Result<()> {
    // Test window focus navigation
    let env = TestEnvironment::new()?;

    env.cleanup_workspace("10")?;
    env.i3_exec("workspace 10")?;

    // Create 2x2 grid
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 41")?;
    std::thread::sleep(Duration::from_millis(800));

    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));
    env.i3_exec("exec --no-startup-id xterm -e /opt/i3mux-test/color-scripts/color-fill.sh 42")?;
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
    env.compare_with_golden("local/focus-navigation.png", &screenshot, &spec)?;

    println!("✓ Focus navigation test passed");

    Ok(())
}
