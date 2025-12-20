// Basic infrastructure tests

use super::common::*;

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
