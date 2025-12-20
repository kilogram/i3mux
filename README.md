# i3mux

[![Tests](https://github.com/kilogram/i3mux/actions/workflows/test.yml/badge.svg)](https://github.com/kilogram/i3mux/actions/workflows/test.yml)

**Bind i3 workspaces to persistent terminal sessions (local or remote)**

i3mux bridges i3 window manager with persistent terminal sessions using `abduco`. Each i3 workspace can be bound to a session (local or remote via SSH), and all terminals opened in that workspace become persistent, reattachable shells.

> **Note**: This project was entirely written by AI (Claude). No human-authored code. The project direction, requirements, and review are provided by a human maintainer.

---

## Why i3mux?

We love tmux. But my muscle memory fights me when using tmux for remote sessions,
and i3 for local ones. i3mux bridges that gap, allowing remote, persistent
sessions with preserved scrollback history (like tmux) but with window management
in i3.

**Use cases:**
- Remote development - survive network drops, pick up exactly where you left off
- Long-running tasks - close the terminal, come back later, output is still there
- Workspace organization - each workspace gets its own isolated session environment

## Features

- **Dead simple**: Single Rust binary, no daemon required
- **Lightning fast**: <1ms overhead per terminal launch
- **SSH integration**: Built-in support for remote sessions
- **Smart terminal detection**: Automatically inherits terminal type from focused window
- **Persistent sessions**: Full scrollback, survives network drops
- **Visual distinction**: Thin titlebars on i3mux terminals only
- **Rofi integration**: Interactive menu for session management

---

## Quick Start

### 1. Install dependencies

**Required:**
- `i3` window manager
- `abduco` - session management (`pacman -S abduco` / `apt install abduco`)

**Optional:**
- `rofi` - for interactive menu
- `notify-send` - for desktop notifications

### 2. Build and install

```bash
git clone https://github.com/kilogram/i3mux.git
cd i3mux
cargo build --release
sudo cp target/release/i3mux /usr/local/bin/

# Optional: Install rofi integration script
sudo cp scripts/i3mux-rofi /usr/local/bin/
sudo chmod +x /usr/local/bin/i3mux-rofi
```

### 3. Configure i3

Add to `~/.config/i3/config`:

```i3config
# i3mux session management (recommended: use rofi script)
bindsym $mod+m exec i3mux-rofi

# Launch smart terminal (respects i3mux binding)
bindsym $mod+Return exec i3mux terminal

# Optional: force normal terminal
bindsym $mod+Shift+Return exec i3-sensible-terminal

# Visual styling for i3mux terminals
default_border none
for_window [title=".*:ws.*"] border normal 2
for_window [title=".*:ws.*"] title_format "%title"
```

Reload i3: `$mod+Shift+r`

### 4. Try it out

1. Press `$mod+m` to open the rofi menu
2. Select "local" or type a remote host (e.g., `user@server.com`)
3. Press `$mod+Return` to open persistent terminals
4. Close a terminal, press `$mod+m` → "Attach" to reattach

---

## Using i3mux-rofi (Recommended)

The included `i3mux-rofi` script provides an interactive menu for all i3mux operations. See [scripts/README.md](scripts/README.md) for full documentation.

**Menu options:**
- **Activate local/remote** - Bind current workspace to a session
- **Detach** - Detach the current workspace session (terminals close, session persists)
- **Attach** - Reattach to a previously detached session
- **List** - View all sessions
- **Kill** - Terminate a session

**Quick workflow:**
```
$mod+m → "local" (activate local session)
$mod+Return (open terminals)
$mod+m → "Detach" (save and close)
$mod+m → "Attach" → select session (resume)
```

---

## CLI Commands

For scripting or when you prefer the command line:

```bash
# Activate i3mux for current workspace
i3mux activate              # local session
i3mux activate --remote user@host  # remote session

# Detach current workspace (save session)
i3mux detach

# Attach to a session
i3mux attach --session <name>
i3mux attach --remote user@host --session <name>

# List sessions
i3mux sessions              # local
i3mux sessions --remote user@host

# Launch terminal (called by i3 keybind)
i3mux terminal

# Kill a session
i3mux kill --session <name>
```

---

## How It Works

```
i3 workspace 2 (bound to user@remote)
├─ Terminal A → ssh user@remote → abduco session ws2-001
├─ Terminal B → ssh user@remote → abduco session ws2-002
├─ Terminal C → normal local terminal (not focused when spawned)
└─ Firefox → unaffected
```

1. **Workspace Binding**: `i3mux activate` binds current workspace to a session
2. **Smart Launching**: `i3mux terminal` checks i3 tree to determine terminal type
3. **Session Management**: Each terminal connects to unique `abduco` socket
4. **Visual Distinction**: i3mux terminals have thin titlebar with session info

State is stored in `~/.config/i3mux/state.json`.

---

## Remote Sessions

### SSH Setup (Required for remote)

For instant terminal attachment, configure SSH ControlMaster:

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

### Remote Prerequisites

`abduco` must be installed on the remote host:
```bash
ssh user@remote-host
sudo pacman -S abduco  # or apt/dnf/brew
```

---

## Troubleshooting

**"abduco not found"**
- Install abduco locally and on remote hosts
- Verify: `which abduco` and `ssh user@host which abduco`

**Terminals close immediately**
- Check SSH keys are set up: `ssh-copy-id user@host`
- Verify abduco is installed

**SSH connections slow**
- Enable ControlMaster (see above)
- Check `~/.ssh/sockets/` directory exists

**Terminal type not detected**
- i3mux supports common terminals (alacritty, kitty, urxvt, st, etc.)
- Focus an i3mux terminal before pressing `$mod+Return`

---

## Documentation

- [INSTALL.md](INSTALL.md) - Detailed installation guide
- [scripts/README.md](scripts/README.md) - Rofi script documentation
- [TESTING.md](TESTING.md) - Running the test suite
- [example-i3-config](example-i3-config) - Complete i3 configuration example

---

## License

GPLv3 - see [LICENSE.txt](LICENSE.txt)

---

## Contributing

PRs welcome! This is an AI-generated codebase under human direction - contributions from both humans and AI are valued.
