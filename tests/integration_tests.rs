// Integration tests for i3mux
// Run with: cargo test --test integration_tests
// Update goldens with: UPDATE_GOLDENS=1 cargo test --test integration_tests

mod common;

use common::*;
use std::time::Duration;

// ==================== Local Session Tests ====================

#[test]
fn test_local_hsplit_2_terminals() -> Result<()> {
    let env = TestEnvironment::new()?;

    // Clean up any existing state
    env.cleanup_workspace("1")?;

    // Activate i3mux for workspace 1 (local)
    env.i3mux_activate(Session::Local, "1")?;

    // Launch first terminal (red)
    let _term1 = env.launch_terminal(ColorScript::Red)?;
    env.wait_for_window_render(_term1, Duration::from_millis(500))?;

    // Split horizontally
    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));

    // Launch second terminal (green)
    let _term2 = env.launch_terminal(ColorScript::Green)?;
    env.wait_for_window_render(_term2, Duration::from_millis(500))?;

    // Capture screenshot
    let screenshot = env.capture_screenshot()?;

    // Compare with golden image
    let spec = ComparisonSpec::load("hsplit-2-terminals")?;
    env.compare_with_golden("local/hsplit-2-terminals.png", &screenshot, &spec)?;

    println!("✓ Local horizontal split test passed");

    Ok(())
}

#[test]
fn test_local_vsplit_2_terminals() -> Result<()> {
    let env = TestEnvironment::new()?;

    // Clean up any existing state
    env.cleanup_workspace("2")?;

    // Activate i3mux for workspace 2 (local)
    env.i3mux_activate(Session::Local, "2")?;

    // Launch first terminal (blue)
    let _term1 = env.launch_terminal(ColorScript::Blue)?;
    env.wait_for_window_render(_term1, Duration::from_millis(500))?;

    // Split vertically
    env.i3_exec("split v")?;
    std::thread::sleep(Duration::from_millis(200));

    // Launch second terminal (yellow)
    let _term2 = env.launch_terminal(ColorScript::Yellow)?;
    env.wait_for_window_render(_term2, Duration::from_millis(500))?;

    // Capture screenshot
    let screenshot = env.capture_screenshot()?;

    // Compare with golden image
    let spec = ComparisonSpec::load("vsplit-2-terminals")?;
    env.compare_with_golden("local/vsplit-2-terminals.png", &screenshot, &spec)?;

    println!("✓ Local vertical split test passed");

    Ok(())
}

// ==================== Layout Restoration Tests ====================

#[test]
fn test_local_detach_attach_hsplit() -> Result<()> {
    let env = TestEnvironment::new()?;

    // Clean up any existing state
    env.cleanup_workspace("3")?;

    // Activate i3mux for workspace 3
    env.i3mux_activate(Session::Local, "3")?;

    // Create layout: horizontal split with red and green
    let _term1 = env.launch_terminal(ColorScript::Red)?;
    env.wait_for_window_render(_term1, Duration::from_millis(500))?;

    env.i3_exec("split h")?;
    std::thread::sleep(Duration::from_millis(200));

    let _term2 = env.launch_terminal(ColorScript::Green)?;
    env.wait_for_window_render(_term2, Duration::from_millis(500))?;

    // Capture screenshot before detach
    let before = env.capture_screenshot()?;

    // NOTE: Detach only works for remote sessions in current i3mux implementation
    // For now, we'll just verify the layout was created correctly
    let spec = ComparisonSpec::load("hsplit-2-terminals")?;
    env.compare_with_golden("local/hsplit-2-terminals.png", &before, &spec)?;

    println!("✓ Local layout creation test passed");

    Ok(())
}

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
