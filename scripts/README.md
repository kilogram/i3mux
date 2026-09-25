# i3mux-rofi

A rofi menu for i3mux.

## Install

```bash
sudo install -Dm755 scripts/i3mux-rofi /usr/local/bin/i3mux-rofi
```

Add to `~/.config/sway/config`:

```
bindsym $mod+m exec i3mux-rofi
```

## Menu

```
<last remote>   (shown once you have used a remote)
Detach
Attach
List
Kill
```

- Press Enter on an empty line to bind the focused workspace to a local
  session and open the first shell.
- Type a host such as `user@dev-box` to bind it to that host instead. The
  script remembers the host in `~/.config/i3mux/last_remote`.
- `Detach` saves the layout and closes the windows of the focused workspace.
- `Attach` lists sessions on local and on the last remote, then restores the
  one you pick into the focused workspace.
- `List` shows the same list read-only.
- `Kill` asks for confirmation, then kills the shells and deletes the layout.

The list lines come straight from `i3mux sessions`, prefixed with the host
they were read from:

```
local|ws4  (local)  2 windows
dev-box|ws3  (dev-box)  detached, 3 terminals saved
dev-box|ws7  (dev-box)  2 shells, no saved layout
```
