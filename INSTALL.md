# i3mux Installation Guide

## Prerequisites

i3mux requires the following to be installed:

### Required

- **i3 window manager** - The window manager i3mux integrates with
- **abduco** - Session management tool (like tmux/screen but simpler)
- **Rust toolchain** - To build i3mux (or use pre-built binary)

### Optional

- **rofi** - For the interactive menu (`i3mux-rofi` script)
- **notify-send** - For desktop notifications

## Installing abduco

abduco is the core session management tool that i3mux uses to maintain persistent sessions.

### Arch Linux
```bash
sudo pacman -S abduco
```

### Debian/Ubuntu
```bash
sudo apt install abduco
```

### Fedora/RHEL
```bash
sudo dnf install abduco
```

### macOS
```bash
brew install abduco
```

### Build from source
```bash
git clone https://github.com/martanne/abduco
cd abduco
make
sudo make install
```

## Installing i3mux

### From source (recommended)

```bash
# Clone the repository
cd ~/src
git clone <repository-url>
cd i3mux

# Build and install
cargo build --release
sudo cp target/release/i3mux /usr/local/bin/

# Optional: Install rofi script
sudo cp scripts/i3mux-rofi /usr/local/bin/
sudo chmod +x /usr/local/bin/i3mux-rofi
```

### Test installation

```bash
# Check abduco is available
which abduco

# Check i3mux is available
which i3mux

# Try activating a local session
i3mux activate
```

## Remote Setup

For remote sessions, abduco must be installed on the remote host as well:

```bash
# SSH to your remote host
ssh user@remote-host

# Install abduco on the remote
sudo pacman -S abduco  # or apt/dnf/etc.

# Verify it's installed
which abduco
```

## i3 Configuration

Add i3mux keybindings to your `~/.config/i3/config`:

```
# i3mux session management
bindsym $mod+m exec i3mux-rofi

# Quick terminal spawn
bindsym $mod+Return exec i3mux terminal
```

See `examples/i3-config-example` for a complete configuration.

## Troubleshooting

### "abduco not found" error

Make sure abduco is installed:
- Locally: `which abduco` should show a path
- Remote: `ssh user@host which abduco` should show a path

### Terminals die immediately

This is usually because:
1. abduco is not installed (see above)
2. SSH connection to remote failed (check SSH keys, permissions)
3. Check logs in `/tmp/i3mux-*.log` for detailed error messages

### SSH issues for remote sessions

Make sure you can SSH to the remote without password:
```bash
# Test SSH connection
ssh user@remote-host echo "connected"

# If prompted for password, set up SSH keys:
ssh-copy-id user@remote-host
```

## Next Steps

- Read `scripts/README.md` for rofi script usage
- Read `examples/i3-config-example` for keybinding examples
- Try the basic workflow: activate → detach → attach
