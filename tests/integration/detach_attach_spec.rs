// Parameterized detach/attach tests driven by spec files
//
// Each spec in golden/specs/restore-*.toml defines:
// - actions: commands to create the layout (after i3mux activate)
// - pre_screenshot: commands to run before capturing screenshot
// - terminal_count: expected number of terminals
//
// This replaces verbose per-layout test functions with a single parameterized test.

use super::common::*;
use rstest::rstest;
use std::time::Duration;

/// Get workspace number for a spec (hash the name to get a unique workspace)
fn workspace_for_spec(spec_name: &str, offset: u32) -> String {
    // Use simple numeric workspaces starting at 60
    let base: u32 = spec_name.bytes().map(|b| b as u32).sum::<u32>() % 20 + 60;
    (base + offset).to_string()
}

/// Parameterized test: detach/attach with layout verification (same workspace)
#[rstest]
#[case("restore-hsplit-2")]
#[case("restore-vsplit-2")]
#[case("restore-tabbed-2")]
#[case("restore-tabbed-3")]
#[case("restore-stacked-2")]
#[case("restore-3way-hsplit")]
#[case("restore-3way-vsplit")]
#[case("restore-tabs-in-hsplit")]
#[case("restore-hsplit-in-tabs")]
#[case("restore-vsplit-in-tabs")]
fn test_detach_attach_spec(#[case] spec_name: &str) -> Result<()> {
    let spec = ComparisonSpec::load(spec_name)?;
    let env = TestEnvironment::new()?;
    let ws = workspace_for_spec(spec_name, 0);
    let session_name = format!("ws{}", ws);

    println!("Testing layout: {} ({})", spec_name, spec.description);

    // Setup workspace
    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Activate i3mux session (creates first terminal)
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), &ws)?;
    std::thread::sleep(Duration::from_secs(2));

    // Execute layout actions from spec
    env.exec_actions(&spec.actions)?;

    // Verify terminal count
    let windows = env.get_workspace_windows()?;
    assert_eq!(
        windows.len(),
        spec.terminal_count,
        "Expected {} terminals, got {}",
        spec.terminal_count,
        windows.len()
    );

    // Run pre-screenshot actions (e.g., focus first tab)
    env.exec_actions(&spec.pre_screenshot)?;

    // Capture screenshot BEFORE detach
    let screenshot_before = env.capture_screenshot()?;
    let golden_name = format!("{}.png", spec_name);
    env.compare_with_golden(&golden_name, &screenshot_before, &spec)?;
    println!("  ✓ Layout verified before detach");

    // Detach
    env.i3mux_detach(&session_name)?;
    std::thread::sleep(Duration::from_millis(500));

    // Verify workspace is empty
    let windows_after_detach = env.get_workspace_windows()?;
    assert_eq!(
        windows_after_detach.len(),
        0,
        "Workspace should be empty after detach"
    );
    println!("  ✓ All terminals detached");

    // Attach
    env.i3mux_attach(Session::Remote("testuser@i3mux-remote-ssh"), &session_name)?;
    std::thread::sleep(Duration::from_secs(3));

    // Verify terminal count restored
    let windows_after_attach = env.get_workspace_windows()?;
    assert_eq!(
        windows_after_attach.len(),
        spec.terminal_count,
        "Should restore {} terminals",
        spec.terminal_count
    );

    // Run pre-screenshot actions again
    env.exec_actions(&spec.pre_screenshot)?;

    // Compare with same golden
    let screenshot_after = env.capture_screenshot()?;
    env.compare_with_golden(&golden_name, &screenshot_after, &spec)?;
    println!("  ✓ Layout verified after attach");

    println!("✓ {} detach/attach test passed", spec_name);
    Ok(())
}

/// Parameterized test: detach/attach with cross-workspace restore
#[rstest]
#[case("restore-hsplit-2")]
#[case("restore-vsplit-2")]
#[case("restore-tabbed-2")]
#[case("restore-tabbed-3")]
#[case("restore-stacked-2")]
#[case("restore-3way-hsplit")]
#[case("restore-3way-vsplit")]
#[case("restore-tabs-in-hsplit")]
#[case("restore-hsplit-in-tabs")]
#[case("restore-vsplit-in-tabs")]
fn test_detach_attach_spec_cross_workspace(#[case] spec_name: &str) -> Result<()> {
    let spec = ComparisonSpec::load(spec_name)?;
    let env = TestEnvironment::new()?;
    let ws1 = workspace_for_spec(spec_name, 100);
    let ws2 = workspace_for_spec(spec_name, 101);
    let session_name = format!("ws{}", ws1);

    println!(
        "Testing cross-workspace: {} ({}) ws{} -> ws{}",
        spec_name, spec.description, ws1, ws2
    );

    // Setup first workspace
    env.cleanup_workspace(&ws1)?;
    env.i3_exec(&format!("workspace {}", ws1))?;

    // Activate and create layout
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), &ws1)?;
    std::thread::sleep(Duration::from_secs(2));
    env.exec_actions(&spec.actions)?;

    // Verify terminal count
    let windows = env.get_workspace_windows()?;
    assert_eq!(windows.len(), spec.terminal_count);

    // Run pre-screenshot actions
    env.exec_actions(&spec.pre_screenshot)?;

    // Capture and verify before detach
    let screenshot_before = env.capture_screenshot()?;
    let golden_name = format!("{}.png", spec_name);
    env.compare_with_golden(&golden_name, &screenshot_before, &spec)?;
    println!("  ✓ Layout verified on workspace {} before detach", ws1);

    // Detach
    env.i3mux_detach(&session_name)?;
    std::thread::sleep(Duration::from_millis(500));

    // Verify empty
    assert_eq!(env.get_workspace_windows()?.len(), 0);

    // Switch to different workspace and attach there
    env.cleanup_workspace(&ws2)?;
    env.i3_exec(&format!("workspace {}", ws2))?;

    env.i3mux_attach(Session::Remote("testuser@i3mux-remote-ssh"), &session_name)?;
    std::thread::sleep(Duration::from_secs(3));

    // Verify terminal count on new workspace
    let windows_after = env.get_workspace_windows()?;
    assert_eq!(windows_after.len(), spec.terminal_count);

    // Run pre-screenshot actions
    env.exec_actions(&spec.pre_screenshot)?;

    // Verify layout matches
    let screenshot_after = env.capture_screenshot()?;
    env.compare_with_golden(&golden_name, &screenshot_after, &spec)?;
    println!(
        "  ✓ Layout verified on workspace {} after cross-workspace attach",
        ws2
    );

    println!("✓ {} cross-workspace test passed", spec_name);
    Ok(())
}
