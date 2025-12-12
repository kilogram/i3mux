# i3mux Testing Infrastructure

Comprehensive containerized testing system for i3mux using Xephyr, Docker/Podman, and screenshot-based verification.

## Quick Start

### Prerequisites

- Rust (for running tests)
- Docker or Podman (for containers)
- podman-compose or docker-compose

### Running Tests

```bash
# Run all tests
cargo test

# Run only integration tests
cargo test --test integration_tests

# Run tests with output
cargo test -- --nocapture

# Run specific test
cargo test test_local_hsplit_2_terminals

# Include ignored tests (SSH/network tests)
cargo test -- --ignored --nocapture
```

### Updating Golden Images

When you make intentional changes to layouts or need to regenerate reference images:

```bash
# Regenerate all golden images
cargo test -- --update-goldens

# Review the updated images in tests/golden/
ls -la tests/golden/local/
ls -la tests/golden/remote/

# Commit the approved images
git add tests/golden/
git commit -m "Update golden images for ..."
```

## Architecture

### Container Setup

The testing infrastructure uses 2 containers:

1. **i3mux-test-xephyr** - Runs Xephyr (nested X server) and i3 window manager
2. **i3mux-remote-ssh** - SSH server simulating a remote host

Both are orchestrated via `docker/docker-compose.yml`.

### Test Flow

1. Test calls `TestEnvironment::new()` - Starts containers if needed (reused across tests)
2. Test executes i3mux commands via the test environment
3. Test captures screenshots using scrot
4. Screenshots are compared with golden images using fuzzy matching (±5px tolerance)
5. On failure, diff images are saved to `tests/test-output/failures/`

### Directory Structure

```
tests/
├── common/              # Shared test infrastructure
│   ├── environment.rs   # Main TestEnvironment orchestrator
│   ├── screenshot.rs    # Image comparison logic
│   ├── comparison_spec.rs  # TOML spec parsing
│   ├── diff_image.rs    # Visual diff generation
│   ├── docker.rs        # Container management
│   ├── i3mux.rs        # i3mux command wrappers
│   └── network.rs      # SSH failure injection
├── docker/             # Container definitions
│   ├── docker-compose.yml
│   ├── Dockerfile.xephyr
│   ├── Dockerfile.remote
│   ├── i3-test-config
│   ├── start-xephyr.sh
│   └── ssh-keys/       # SSH keys for passwordless auth
├── color-scripts/      # Terminal color fill scripts
│   └── color-fill.sh
├── golden/             # Golden reference images
│   ├── local/          # Local session screenshots
│   ├── remote/         # Remote session screenshots
│   └── specs/          # TOML comparison specifications
├── screenshots/        # Temporary screenshots (gitignored)
├── test-output/        # Test failure artifacts (gitignored)
│   └── failures/       # Diff images and reports
└── integration_tests.rs  # Integration test suite
```

## Writing Tests

### Basic Test Structure

```rust
#[test]
fn test_my_layout() -> Result<()> {
    let env = TestEnvironment::new()?;

    // Clean workspace
    env.cleanup_workspace("1")?;

    // Activate i3mux
    env.i3mux_activate(Session::Local, "1")?;

    // Create layout
    env.launch_terminal(ColorScript::Red)?;
    env.i3_exec("split h")?;
    env.launch_terminal(ColorScript::Green)?;

    // Verify screenshot
    let screenshot = env.capture_screenshot()?;
    let spec = ComparisonSpec::load("my-layout")?;
    env.compare_with_golden("local/my-layout.png", &screenshot, &spec)?;

    Ok(())
}
```

### Testing Remote Sessions

```rust
#[test]
fn test_remote_session() -> Result<()> {
    let env = TestEnvironment::new()?;

    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "2")?;

    // SSH commands will be executed via ControlMaster
    let _term = env.launch_terminal(ColorScript::Blue)?;
    env.wait_for_ssh_connection(_term, Duration::from_secs(3))?;

    Ok(())
}
```

### Testing Network Failures

```rust
#[test]
fn test_with_packet_loss() -> Result<()> {
    let env = TestEnvironment::new()?;

    // Inject 20% packet loss
    env.inject_packet_loss(20)?;

    // Test should still work, just slower
    env.i3mux_activate(Session::Remote("testuser@i3mux-remote-ssh"), "3")?;

    // Clean up
    env.clear_network_rules()?;

    Ok(())
}
```

## Comparison Specs

Comparison specs define how screenshots should be compared. They're TOML files in `tests/golden/specs/`.

### Example Spec

```toml
name = "Horizontal split - 2 terminals"

# Exact color matching for terminal regions
[[exact_regions]]
x = 0
y = 20
width = 958
height = 1058
expected_color = [170, 0, 0]  # Dark red ANSI background

[[exact_regions]]
x = 962
y = 20
width = 958
height = 1058
expected_color = [0, 170, 0]  # Dark green ANSI background

# Fuzzy matching for window borders
[fuzzy_boundaries]
tolerance_px = 5              # ±5 pixel tolerance for borders
max_diff_pixels = 2000        # Max total diff pixels allowed
max_diff_percentage = 1.5     # Max 1.5% diff allowed
```

## Troubleshooting

### Containers won't start

```bash
# Check if containers are running
podman ps  # or: docker ps

# View container logs
podman logs i3mux-test-xephyr
podman logs i3mux-remote-ssh

# Restart containers
cd tests/docker
podman-compose down
podman-compose up -d
```

### Screenshots don't match

When a test fails with screenshot mismatches:

1. Check `tests/test-output/failures/<test-name>/<timestamp>/`
2. Review `comparison.png` (side-by-side comparison)
3. Review `diff.png` (highlighted differences)
4. Check if the diff is intentional or a bug
5. If intentional, regenerate goldens: `cargo test -- --update-goldens`

### SSH tests failing

```bash
# Check SSH connectivity from Xephyr container
podman exec i3mux-test-xephyr ssh testuser@i3mux-remote-ssh echo ok

# Check SSH keys
ls -la tests/docker/ssh-keys/

# Regenerate SSH keys if needed
cd tests/docker/ssh-keys
rm -f id_rsa id_rsa.pub
ssh-keygen -t rsa -b 2048 -f id_rsa -N "" -C "i3mux-test-key"
```

## Performance

- First test run: ~30s (container startup)
- Subsequent tests: ~2-5s each (containers reused)
- Full test suite: ~5-10 minutes

## CI/CD

Tests run automatically on GitHub Actions. See `.github/workflows/test.yml`.

CI will fail if:
- Any test fails
- Golden images are uncommitted (after running with `--update-goldens`)
- Container build fails
