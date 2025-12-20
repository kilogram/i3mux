// Network failure tests

use super::common::*;
use std::time::Duration;

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
