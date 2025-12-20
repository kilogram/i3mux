// Nested layout tests: tabs in splits, splits in tabs, and complex nested layouts

use super::common::*;
use super::{should_ignore_session, workspace_for_session};
use rstest::rstest;
use std::time::Duration;

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
