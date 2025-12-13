# i3mux-rofi

Simple rofi interface for i3mux session management.

## Installation

1. Copy `i3mux-rofi` to somewhere in your PATH or a known location
2. Make it executable: `chmod +x i3mux-rofi`
3. Add to your i3 config:

```
bindsym $mod+m exec /path/to/i3mux-rofi
```

## Usage

Press `Mod+m` to open the rofi menu. You'll see:

```
Activate: local
Activate: last-server.com     (if you've used a remote before)
Detach
Attach
List Sessions
Kill Session
```

### Activate a Session

- Select `Activate: local` to activate the current workspace locally
- **Edit** `Activate: local` to `Activate: user@server.com` and press Enter to activate remote
- The script remembers your last remote for quick access

### Attach to a Session

- Select `Attach` to see all available sessions (local + last remote)
- Sessions show as `[host] sessionname - N terminals`
- Select one to attach

### Detach

- Select `Detach` to detach the current workspace
- Session is saved and terminals are closed
- You can attach again later from any machine

### Other Operations

- `List Sessions`: Read-only view of all sessions
- `Kill Session`: Select and confirm to delete a session

## Tips

- The script remembers your last used remote host in `~/.config/i3mux/last_remote`
- You can edit any "Activate:" line in the menu to specify a different host
- For quick access, just type part of the menu item to filter (rofi standard behavior)

## Examples

**Activate local session:**
1. `Mod+m` → select "Activate: local" → Enter

**Activate remote session:**
1. `Mod+m` → edit "Activate: local" to "Activate: user@myserver" → Enter

**Quick activate last remote:**
1. `Mod+m` → select "Activate: myserver.com" (if shown) → Enter

**Attach to session:**
1. `Mod+m` → select "Attach" → select session from list → Enter

**Detach:**
1. `Mod+m` → select "Detach" → Enter
