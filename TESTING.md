# Testing i3mux

Comprehensive testing strategy for a window manager integration tool.

## 1. Unit Tests

Test pure logic without i3 dependency:

```bash
cargo test
```

**What's tested:**
- ✅ Config serialization/deserialization
- ✅ Socket ID generation and incrementing
- ✅ Socket naming format
- ✅ Session type detection (local vs remote)
- ✅ Workspace session lookups

**Coverage:** ~40% (pure business logic)

## 2. Integration Tests (Manual)

These require a running i3 session.

### Setup Test Environment

```bash
# Install dependencies
sudo pacman -S abduco i3 xterm

# Build i3mux
cargo build --release
sudo cp target/release/i3mux /usr/local/bin/

# Add to i3 config
cat example-i3-config >> ~/.config/i3/config
i3-msg reload
```

### Test Suite A: Basic Activation

```bash
# Switch to empty workspace
i3-msg 'workspace 8'

# Activate local session
i3mux activate local
# Expected: ✓ Workspace 8 activated with session: local
# Expected: First terminal launches with titlebar "⚡ local:ws8-001"

# Check state
i3mux list
# Expected:
# Active i3mux sessions:
# Workspace 8: local (1 terminals)
#   • ws8-001

# Deactivate
i3mux deactivate
# Expected: ✓ Workspace 8 deactivated
# Expected: Terminal closes
```

### Test Suite B: Smart Terminal Launching

```bash
# Activate workspace
i3-msg 'workspace 7'
i3mux activate local

# Test 1: Empty workspace → i3mux terminal
i3-msg 'exec i3mux terminal'
# Expected: i3mux terminal launches (titlebar)

# Test 2: Focus i3mux terminal, split → i3mux terminal
i3-msg 'exec i3mux terminal'
# Expected: Another i3mux terminal

# Test 3: Open browser
i3-msg 'exec firefox'
sleep 2
i3-msg 'focus'

# Test 4: Focus browser, open terminal → normal terminal
i3-msg 'exec i3mux terminal'
# Expected: Normal terminal (no titlebar)

# Test 5: Focus normal terminal, split → normal terminal
i3-msg 'exec i3mux terminal'
# Expected: Another normal terminal

# Verify mix
i3-msg '[title=".*:ws.*"] focus'  # Should focus i3mux terminals
# Should have 2 i3mux terminals + 2 normal + browser
```

### Test Suite C: Remote Sessions

Requires SSH access to a remote host.

```bash
# Activate remote session
i3-msg 'workspace 6'
i3mux activate user@remotehost

# Open terminal
i3-msg 'exec i3mux terminal'
# Expected: SSH connection, abduco attaches
# Expected: Shell prompt from remote host

# Check session on remote
# (in the i3mux terminal)
$ hostname
# Expected: remotehost

$ abduco
# Expected: ws6-001 session listed

# Open second terminal
i3-msg 'exec i3mux terminal'
# Expected: Fast connection (SSH ControlMaster reuse)

# Check local state
i3mux list
# Expected:
# Workspace 6: user@remotehost (2 terminals)
#   • ws6-001
#   • ws6-002
```

### Test Suite D: Session Persistence

```bash
# Activate local session
i3-msg 'workspace 5'
i3mux activate local
i3-msg 'exec i3mux terminal'

# In the terminal, start a long-running process
$ sleep 1000 &
$ jobs
# [1]+  Running    sleep 1000 &

# Close the terminal window
i3-msg 'kill'

# Check abduco on host
$ abduco
# Expected: ws5-001 session still exists

# Reattach
i3-msg 'exec i3mux terminal'
# In new terminal:
$ jobs
# Expected: [1]+  Running    sleep 1000 &
# Session persisted!
```

### Test Suite E: Visual Indicators

```bash
# Activate workspace
i3mux activate local
i3-msg 'exec i3mux terminal'

# Check titlebar
# Expected: 2px border with "⚡ local:ws5-001" or similar

# Open normal terminal
i3-msg 'exec i3-sensible-terminal'
# Expected: No border/titlebar

# Visual difference should be obvious
```

### Test Suite F: Multi-Workspace

```bash
# Workspace 1: Not activated
i3-msg 'workspace 1'
i3-msg 'exec i3mux terminal'
# Expected: Normal terminal

# Workspace 2: Local
i3-msg 'workspace 2'
i3mux activate local
i3-msg 'exec i3mux terminal'
# Expected: i3mux terminal

# Workspace 3: Remote
i3-msg 'workspace 3'
i3mux activate user@host
i3-msg 'exec i3mux terminal'
# Expected: i3mux terminal to remote

# List all
i3mux list
# Expected:
# Active i3mux sessions:
# Workspace 2: local (1 terminals)
# Workspace 3: user@host (1 terminals)
```

## 3. Automated Integration Tests (Xephyr)

Run i3 in nested X server for safe automated testing.

### Setup

```bash
# Install Xephyr
sudo pacman -S xorg-server-xephyr

# Create test i3 config
mkdir -p ~/.config/i3mux-test
cp ~/.config/i3/config ~/.config/i3mux-test/config
echo "exec i3-msg 'workspace 1'" >> ~/.config/i3mux-test/config
```

### Run Tests in Xephyr

```bash
#!/bin/bash
# test-xephyr.sh

# Start Xephyr
Xephyr :1 -screen 1920x1080 &
XEPHYR_PID=$!

# Start i3 in Xephyr
DISPLAY=:1 i3 -c ~/.config/i3mux-test/config &
I3_PID=$!

sleep 2

# Run test commands
DISPLAY=:1 i3mux activate local
DISPLAY=:1 i3mux list
DISPLAY=:1 i3-msg 'exec i3mux terminal'

sleep 2

# Check results
DISPLAY=:1 i3-msg -t get_tree | jq '.nodes[].nodes[]|select(.name=="1")|.nodes|length'
# Expected: 1 (one window)

# Cleanup
kill $I3_PID
kill $XEPHYR_PID
```

## 4. Property-Based Testing (Future)

For more rigorous testing, consider property-based tests:

```rust
#[cfg(test)]
mod property_tests {
    use super::*;
    use quickcheck::QuickCheck;

    #[test]
    fn prop_socket_ids_unique() {
        fn check(workspace: u8, count: u8) -> bool {
            let mut config = Config::default();
            config.workspaces.insert(
                workspace.to_string(),
                WorkspaceSession {
                    session_type: "local".to_string(),
                    host: "local".to_string(),
                    next_socket_id: 1,
                    sockets: HashMap::new(),
                },
            );

            let mut sockets = HashSet::new();
            for _ in 0..count.min(100) {
                if let Ok(socket) = config.next_socket(&workspace.to_string()) {
                    if sockets.contains(&socket) {
                        return false; // Duplicate found!
                    }
                    sockets.insert(socket);
                }
            }
            true
        }

        QuickCheck::new().quickcheck(check as fn(u8, u8) -> bool);
    }
}
```

## 5. Performance Testing

### Startup Time

```bash
# Measure i3mux terminal launch time
time i3mux terminal

# Expected: < 100ms
# Actual: ~50ms (mostly terminal startup)
```

### i3 IPC Overhead

```bash
# Benchmark i3 state queries
hyperfine 'i3mux list' 'i3-msg -t get_workspaces'

# Expected: Similar performance (both use i3 IPC)
```

### SSH Connection Reuse

```bash
# Without ControlMaster (slow)
time ssh user@remote 'abduco -A test bash -c "exit"'
# Expected: ~500ms (full handshake)

# With ControlMaster (fast)
time ssh user@remote 'abduco -A test bash -c "exit"'
# Expected: ~50ms (reuses connection)
```

## 6. Regression Testing

Create a test checklist for each release:

- [ ] Config loads/saves without corruption
- [ ] Socket IDs increment correctly
- [ ] Local sessions work
- [ ] Remote sessions work
- [ ] SSH ControlMaster reuses connections
- [ ] Titlebars appear on i3mux terminals
- [ ] Normal terminals work in i3mux workspace
- [ ] Mixed windows (browser + terminals) work
- [ ] Session persistence (close/reopen)
- [ ] Multi-workspace independence
- [ ] Deactivation cleans up properly

## 7. Known Issues to Test

1. **Terminal detection**: Currently uses title matching
   - Test: Different terminal emulators (alacritty, kitty, urxvt)

2. **Mark tracking**: Not using i3 marks properly yet
   - Test: Manual mark manipulation doesn't break detection

3. **Race condition**: 100ms sleep for window marking
   - Test: Rapid terminal launches (10 terminals in 1s)

## 8. Debugging Tips

### Enable verbose logging

```rust
// In main.rs, add debug output
eprintln!("DEBUG: Workspace {} is i3mux: {}", ws_name, is_bound);
```

### Check i3 state

```bash
# See all windows
i3-msg -t get_tree | jq '.. | select(.window?) | {window, name, marks, class}'

# See focused window
i3-msg -t get_tree | jq '.. | select(.focused? == true)'

# See workspaces
i3-msg -t get_workspaces | jq '.[]|{num,name,focused}'
```

### Check abduco sessions

```bash
# Local
abduco

# Remote
ssh user@host abduco

# Attach manually
abduco -a ws2-001
```

### Check SSH connections

```bash
# Active control connections
ls -la ~/.ssh/sockets/

# Test connection
ssh -O check user@host
```

## Summary

**Recommended testing workflow:**

1. Run unit tests: `cargo test` (fast, always)
2. Manual integration tests: Test suite A-E (before release)
3. Xephyr automated tests: Full workflow (CI/CD)
4. Regression checklist: Every release

**Coverage:**
- Unit tests: Pure logic
- Integration: Real i3 interaction
- Manual: User experience
- Automated: Workflow correctness
