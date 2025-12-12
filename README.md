# i3mux

**Bind i3 workspaces to persistent terminal sessions (local or remote)**

i3mux bridges i3 window manager with persistent terminal sessions using `abduco`. Each i3 workspace can be bound to a session (local or remote via SSH), and all terminals opened in that workspace become persistent, reattachable shells.

## Features

- 🚀 **Dead simple**: Single Rust binary, no daemon required
- ⚡ **Lightning fast**: <1ms overhead per terminal launch
- 🔌 **SSH integration**: Built-in support for remote sessions
- 🪟 **Smart terminal detection**: Automatically inherits terminal type from focused window
- 💾 **Persistent sessions**: Full scrollback, survives network drops
- 🎨 **Visual distinction**: Thin titlebars on i3mux terminals only

## Installation

### Prerequisites

- Rust toolchain (for building)
- `i3` window manager
- `abduco` - session management (`pacman -S abduco` / `apt install abduco`)
- SSH (for remote sessions)

### Build

```bash
cargo build --release
sudo cp target/release/i3mux /usr/local/bin/
```

## Quick Start

### 1. Configure i3

Add to `~/.config/i3/config`:

```i3config
# Launch smart terminal (respects i3mux binding)
bindsym $mod+Return exec i3mux terminal

# Activate/deactivate i3mux for current workspace
bindsym $mod+m exec i3mux activate $(rofi -dmenu -p 'i3mux session (local or user@host):')
bindsym $mod+Shift+m exec i3mux deactivate

# Optional: force normal terminal
bindsym $mod+Shift+Return exec i3-sensible-terminal

# Visual styling for i3mux terminals
default_border none
for_window [title=".*:ws.*"] border normal 2
for_window [title=".*:ws.*"] title_format "⚡ %title"
```

Reload i3: `$mod+Shift+r`

### 2. Activate a workspace

**Local session:**
```bash
$mod+m → type: "local"
```

**Remote session:**
```bash
$mod+m → type: "user@remote.host"
```

### 3. Open terminals

```bash
$mod+Return  # Opens i3mux terminal (inherits from focused window)
```

**Smart behavior:**
- Empty workspace → i3mux terminal
- Focused on i3mux terminal → new i3mux terminal
- Focused on normal terminal → new normal terminal
- Focused on browser/app → normal terminal

## Usage

### CLI Commands

```bash
# Activate i3mux for current workspace
i3mux activate local
i3mux activate user@remote.host

# Deactivate current workspace
i3mux deactivate

# Launch terminal (called by i3 keybind)
i3mux terminal

# List all sessions
i3mux list

# Cleanup dead sockets
i3mux cleanup
i3mux cleanup 2  # specific workspace
```

### SSH Configuration

For optimal performance with remote sessions, configure SSH ControlMaster:

```bash
# ~/.ssh/config
Host *
  ControlMaster auto
  ControlPath ~/.ssh/sockets/%r@%h:%p
  ControlPersist 10m
```

Create socket directory:
```bash
mkdir -p ~/.ssh/sockets
```

This reuses SSH connections across terminals for instant attachment.

## How It Works

1. **Workspace Binding**: `i3mux activate` binds current workspace to a session
2. **State Tracking**: Config stored in `~/.config/i3mux/state.json`
3. **Smart Launching**: `i3mux terminal` checks i3 tree to determine terminal type
4. **Session Management**: Each terminal connects to unique `abduco` socket
5. **Visual Distinction**: i3mux terminals have thin titlebar with session info

**Architecture:**
```
i3 workspace 2 (bound to user@remote)
├─ Terminal A → ssh user@remote → abduco -A /tmp/ws2-001 bash
├─ Terminal B → ssh user@remote → abduco -A /tmp/ws2-002 bash
├─ Terminal C → normal local terminal
└─ Firefox → unaffected
```

## State File

Located at `~/.config/i3mux/state.json`:

```json
{
  "workspaces": {
    "2": {
      "session_type": "remote",
      "host": "user@remote.host",
      "next_socket_id": 3,
      "sockets": {
        "ws2-001": { "socket_id": "ws2-001", "window_id": null },
        "ws2-002": { "socket_id": "ws2-002", "window_id": null }
      }
    }
  }
}
```

## Visual Customization

### Titlebar Height

```i3config
for_window [title=".*:ws.*"] border normal 1  # 1px titlebar
for_window [title=".*:ws.*"] border normal 3  # 3px titlebar
```

### Title Format

```i3config
# Show session + socket
for_window [title=".*:ws.*"] title_format "⚡ %title"

# Minimal
for_window [title=".*:ws.*"] title_format "%title"

# Custom emoji
for_window [title=".*:ws.*"] title_format "🖥️ %title"
```

### Terminal Classes

If your terminal isn't detected, add it to `src/main.rs:185`:

```rust
let terminal_classes = ["URxvt", "Alacritty", "kitty", "st", "YourTerminal"];
```

## Workflow Examples

### Remote Development

```bash
# Workspace 1: Local work
$mod+1
# Normal terminals, no i3mux

# Workspace 2: Remote dev server
$mod+2
$mod+m → "dev@server"
$mod+Return  # vim
$mod+Return  # build terminal
$mod+Return  # test terminal

# Network drops... all sessions persist on remote
# Reconnect: terminals auto-reattach to same shells
```

### Mixed Workspace

```bash
$mod+3
$mod+m → "user@remote"
$mod+Return  # i3mux terminal (remote shell)
$mod+d → firefox  # normal window
focus firefox, $mod+Return  # normal local terminal
```

## Troubleshooting

**Terminals not marked as i3mux:**
- Check titlebar contains session name (e.g., `user@remote:ws2-001`)
- Verify i3 config has `for_window` rules loaded

**SSH connections slow:**
- Enable SSH ControlMaster (see above)
- Check `~/.ssh/sockets/` directory exists

**Can't detect i3mux terminals:**
- Currently uses title matching (contains `ws` + number)
- Future: proper mark tracking

## Roadmap

- [ ] Proper i3 mark tracking (vs title matching)
- [ ] Auto-reconnect on network restore
- [ ] Socket cleanup on window close
- [ ] Session naming (beyond just host)
- [ ] GUI activation dialog (rofi/dmenu wrapper)
- [ ] Workspace restore on i3 restart

## License

GPLv3 (see LICENSE.txt)

## Contributing

PRs welcome! This is a minimal viable implementation - lots of room for polish.
