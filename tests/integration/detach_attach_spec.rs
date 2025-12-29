// Tiered detach/attach tests driven by spec files
//
// Test Tiers:
// - T1 (default): All specs × sessions × WMs (same-WM), ~40 tests, ~60s
// - T2 (ignored): Full matrix with cross-WM + op-order, ~120 tests
//
// Run T1 only:    cargo test --test integration
// Run T1 + T2:    I3MUX_FULL_MATRIX=1 cargo test --test integration -- --include-ignored

use super::common::*;
use rstest::rstest;
use std::time::Duration;

/// Get workspace number for a test case (unique per spec + session + wm combination)
fn workspace_for_test(spec_name: &str, session: SessionType, wm: WmType, offset: u32) -> String {
    let base: u32 = spec_name.bytes().map(|b| b as u32).sum::<u32>() % 30;
    let session_offset = match session {
        SessionType::Local => 0,
        SessionType::Remote => 100,
    };
    let wm_offset = match wm {
        WmType::I3 => 0,
        WmType::Sway => 200,
    };
    (base + session_offset + wm_offset + offset).to_string()
}

/// Convert tier SessionType to environment Session
fn to_session(session_type: SessionType) -> Session {
    match session_type {
        SessionType::Local => Session::Local,
        SessionType::Remote => Session::Remote("testuser@i3mux-remote-ssh"),
    }
}

/// Convert tier WmType to docker TestWmType
fn to_test_wm_type(wm: WmType) -> TestWmType {
    match wm {
        WmType::I3 => TestWmType::I3,
        WmType::Sway => TestWmType::Sway,
    }
}

// =============================================================================
// T1: Default tests (same-WM detach/attach)
// 10 specs × 2 WMs = 20 tests (Remote sessions only - Local cannot detach)
// =============================================================================

#[rstest]
// restore-hsplit-2
#[case("restore-hsplit-2", WmType::I3)]
#[case("restore-hsplit-2", WmType::Sway)]
// restore-vsplit-2
#[case("restore-vsplit-2", WmType::I3)]
#[case("restore-vsplit-2", WmType::Sway)]
// restore-tabbed-2
#[case("restore-tabbed-2", WmType::I3)]
#[case("restore-tabbed-2", WmType::Sway)]
// restore-tabbed-3
#[case("restore-tabbed-3", WmType::I3)]
#[case("restore-tabbed-3", WmType::Sway)]
// restore-stacked-2
#[case("restore-stacked-2", WmType::I3)]
#[case("restore-stacked-2", WmType::Sway)]
// restore-3way-hsplit
#[case("restore-3way-hsplit", WmType::I3)]
#[case("restore-3way-hsplit", WmType::Sway)]
// restore-3way-vsplit
#[case("restore-3way-vsplit", WmType::I3)]
#[case("restore-3way-vsplit", WmType::Sway)]
// restore-tabs-in-hsplit
#[case("restore-tabs-in-hsplit", WmType::I3)]
#[case("restore-tabs-in-hsplit", WmType::Sway)]
// restore-hsplit-in-tabs
#[case("restore-hsplit-in-tabs", WmType::I3)]
#[case("restore-hsplit-in-tabs", WmType::Sway)]
// restore-vsplit-in-tabs
#[case("restore-vsplit-in-tabs", WmType::I3)]
#[case("restore-vsplit-in-tabs", WmType::Sway)]
fn test_restore_same_wm(
    #[case] spec_name: &str,
    #[case] wm: WmType,
) -> Result<()> {
    let spec = ComparisonSpec::load(spec_name)?;
    let env = TestEnvironment::new()?;
    // Remote sessions only - Local sessions cannot be detached/attached
    let session_type = SessionType::Remote;
    let ws = workspace_for_test(spec_name, session_type, wm, 0);
    let session_name = format!("ws{}", ws);
    let session = to_session(session_type);

    println!(
        "T1 Test: {} | {} | {} | same-WM",
        spec_name, session_type, wm
    );

    // Verify we're testing the expected WM
    let actual_wm = env.wm_type();
    let expected_wm = to_test_wm_type(wm);
    if actual_wm != expected_wm {
        println!(
            "  Skipping: test expects {:?} but environment is {:?}",
            expected_wm, actual_wm
        );
        return Ok(());
    }

    // Setup workspace
    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Activate i3mux session
    env.i3mux_activate(session.clone(), &ws)?;
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

    // Run pre-screenshot actions
    env.exec_actions(&spec.pre_screenshot)?;

    // Capture and verify screenshot BEFORE detach
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

    // Attach (same WM)
    env.i3mux_attach(session, &session_name)?;
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

    println!("✓ {} same-WM test passed", spec_name);
    Ok(())
}

// =============================================================================
// T2: Full matrix tests (cross-WM scenarios)
// 10 specs × 2 directions = 20 tests (Remote sessions only)
// Requires I3MUX_FULL_MATRIX=1 and --include-ignored
// =============================================================================

#[rstest]
// Cross-WM: Create in i3, attach in Sway
#[case("restore-hsplit-2", WmType::I3, WmType::Sway)]
#[case("restore-vsplit-2", WmType::I3, WmType::Sway)]
#[case("restore-tabbed-2", WmType::I3, WmType::Sway)]
#[case("restore-tabbed-3", WmType::I3, WmType::Sway)]
#[case("restore-stacked-2", WmType::I3, WmType::Sway)]
#[case("restore-3way-hsplit", WmType::I3, WmType::Sway)]
#[case("restore-3way-vsplit", WmType::I3, WmType::Sway)]
#[case("restore-tabs-in-hsplit", WmType::I3, WmType::Sway)]
#[case("restore-hsplit-in-tabs", WmType::I3, WmType::Sway)]
#[case("restore-vsplit-in-tabs", WmType::I3, WmType::Sway)]
// Cross-WM: Create in Sway, attach in i3
#[case("restore-hsplit-2", WmType::Sway, WmType::I3)]
#[case("restore-vsplit-2", WmType::Sway, WmType::I3)]
#[case("restore-tabbed-2", WmType::Sway, WmType::I3)]
#[case("restore-tabbed-3", WmType::Sway, WmType::I3)]
#[case("restore-stacked-2", WmType::Sway, WmType::I3)]
#[case("restore-3way-hsplit", WmType::Sway, WmType::I3)]
#[case("restore-3way-vsplit", WmType::Sway, WmType::I3)]
#[case("restore-tabs-in-hsplit", WmType::Sway, WmType::I3)]
#[case("restore-hsplit-in-tabs", WmType::Sway, WmType::I3)]
#[case("restore-vsplit-in-tabs", WmType::Sway, WmType::I3)]
#[ignore = "T2: cross-WM tests, run with I3MUX_FULL_MATRIX=1"]
fn test_restore_cross_wm(
    #[case] spec_name: &str,
    #[case] create_wm: WmType,
    #[case] attach_wm: WmType,
) -> Result<()> {
    // Skip unless full matrix is enabled
    if !is_full_matrix_enabled() {
        println!("Skipping T2 test (I3MUX_FULL_MATRIX not set)");
        return Ok(());
    }

    let spec = ComparisonSpec::load(spec_name)?;
    let dual_env = DualTestEnvironment::new()?;
    // Remote sessions only - Local sessions cannot be detached/attached
    let session_type = SessionType::Remote;
    let ws = workspace_for_test(spec_name, session_type, create_wm, 500);
    let session_name = format!("ws{}", ws);
    let session = to_session(session_type);

    println!(
        "T2 Test: {} | {} | {} -> {}",
        spec_name, session_type, create_wm, attach_wm
    );

    let create_env = dual_env.for_wm(to_test_wm_type(create_wm));
    let attach_env = dual_env.for_wm(to_test_wm_type(attach_wm));

    // Setup workspace on create WM
    create_env.cleanup_workspace(&ws)?;
    create_env.i3_exec(&format!("workspace {}", ws))?;

    // Activate and create layout
    create_env.i3mux_activate(session.clone(), &ws)?;
    std::thread::sleep(Duration::from_secs(2));
    create_env.exec_actions(&spec.actions)?;

    // Verify terminal count
    let windows = create_env.get_workspace_windows()?;
    assert_eq!(windows.len(), spec.terminal_count);

    // Capture screenshot on create WM
    create_env.exec_actions(&spec.pre_screenshot)?;
    let screenshot_before = create_env.capture_screenshot()?;
    let golden_name = format!("{}.png", spec_name);
    create_env.compare_with_golden(&golden_name, &screenshot_before, &spec)?;
    println!("  ✓ Layout verified on {} before detach", create_wm);

    // Detach from create WM
    create_env.i3mux_detach(&session_name)?;
    std::thread::sleep(Duration::from_millis(500));

    // Verify empty on create WM
    assert_eq!(create_env.get_workspace_windows()?.len(), 0);
    println!("  ✓ Detached from {}", create_wm);

    // Setup workspace on attach WM
    attach_env.cleanup_workspace(&ws)?;
    attach_env.i3_exec(&format!("workspace {}", ws))?;

    // Attach from different WM
    attach_env.i3mux_attach(session, &session_name)?;
    std::thread::sleep(Duration::from_secs(3));

    // Verify terminal count restored
    let windows_after = attach_env.get_workspace_windows()?;
    assert_eq!(windows_after.len(), spec.terminal_count);

    // Capture screenshot on attach WM
    attach_env.exec_actions(&spec.pre_screenshot)?;
    let screenshot_after = attach_env.capture_screenshot()?;
    attach_env.compare_with_golden(&golden_name, &screenshot_after, &spec)?;
    println!("  ✓ Layout verified on {} after attach", attach_wm);

    println!("✓ {} cross-WM test passed ({} -> {})", spec_name, create_wm, attach_wm);
    Ok(())
}

// =============================================================================
// T2: Operations after attach tests
// Tests that layout operations work correctly after restoring a session
// 10 specs × 2 WMs = 20 tests (Remote sessions only)
// =============================================================================

#[rstest]
#[case("restore-hsplit-2", WmType::I3)]
#[case("restore-hsplit-2", WmType::Sway)]
#[case("restore-vsplit-2", WmType::I3)]
#[case("restore-vsplit-2", WmType::Sway)]
#[case("restore-tabbed-2", WmType::I3)]
#[case("restore-tabbed-2", WmType::Sway)]
#[case("restore-tabbed-3", WmType::I3)]
#[case("restore-tabbed-3", WmType::Sway)]
#[case("restore-stacked-2", WmType::I3)]
#[case("restore-stacked-2", WmType::Sway)]
#[case("restore-3way-hsplit", WmType::I3)]
#[case("restore-3way-hsplit", WmType::Sway)]
#[case("restore-3way-vsplit", WmType::I3)]
#[case("restore-3way-vsplit", WmType::Sway)]
#[case("restore-tabs-in-hsplit", WmType::I3)]
#[case("restore-tabs-in-hsplit", WmType::Sway)]
#[case("restore-hsplit-in-tabs", WmType::I3)]
#[case("restore-hsplit-in-tabs", WmType::Sway)]
#[case("restore-vsplit-in-tabs", WmType::I3)]
#[case("restore-vsplit-in-tabs", WmType::Sway)]
#[ignore = "T2: ops-after tests, run with I3MUX_FULL_MATRIX=1"]
fn test_restore_ops_after_attach(
    #[case] spec_name: &str,
    #[case] wm: WmType,
) -> Result<()> {
    // Skip unless full matrix is enabled
    if !is_full_matrix_enabled() {
        println!("Skipping T2 ops-after test (I3MUX_FULL_MATRIX not set)");
        return Ok(());
    }

    let spec = ComparisonSpec::load(spec_name)?;
    let env = TestEnvironment::new()?;
    // Remote sessions only - Local sessions cannot be detached/attached
    let session_type = SessionType::Remote;
    let ws = workspace_for_test(spec_name, session_type, wm, 600);
    let session_name = format!("ws{}", ws);
    let session = to_session(session_type);

    println!(
        "T2 Test (ops-after): {} | {} | {}",
        spec_name, session_type, wm
    );

    // Verify WM type
    let actual_wm = env.wm_type();
    let expected_wm = to_test_wm_type(wm);
    if actual_wm != expected_wm {
        println!("  Skipping: test expects {:?} but environment is {:?}", expected_wm, actual_wm);
        return Ok(());
    }

    // Setup
    env.cleanup_workspace(&ws)?;
    env.i3_exec(&format!("workspace {}", ws))?;

    // Activate session (creates first terminal)
    env.i3mux_activate(session.clone(), &ws)?;
    std::thread::sleep(Duration::from_secs(2));

    // Detach immediately (just the initial terminal)
    env.i3mux_detach(&session_name)?;
    std::thread::sleep(Duration::from_millis(500));

    // Attach
    env.i3mux_attach(session, &session_name)?;
    std::thread::sleep(Duration::from_secs(2));

    // Now execute layout actions AFTER attach
    env.exec_actions(&spec.actions)?;

    // Verify terminal count
    let windows = env.get_workspace_windows()?;
    assert_eq!(
        windows.len(),
        spec.terminal_count,
        "Expected {} terminals after ops-after-attach",
        spec.terminal_count
    );

    // Run pre-screenshot actions
    env.exec_actions(&spec.pre_screenshot)?;

    // Compare with golden
    let screenshot = env.capture_screenshot()?;
    let golden_name = format!("{}.png", spec_name);
    env.compare_with_golden(&golden_name, &screenshot, &spec)?;

    println!("  ✓ Layout verified after ops-after-attach");
    println!("✓ {} ops-after test passed", spec_name);
    Ok(())
}
