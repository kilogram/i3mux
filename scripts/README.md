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

In an unbound workspace:

```
<last remote>   (shown once you have used a remote)
Attach
List
Kill
```

In a bound workspace:

```
Resume   (shown only when the workspace is resumable)
List
Detach
Kill
```

- Press Enter on an empty line to bind the focused workspace to a local
  session and open the first shell.
- Type a host such as `user@dev-box` to bind it to that host instead. The
  script remembers the host in `~/.config/i3mux/last_remote`.
- `Resume` runs `i3mux terminal` in a workspace whose windows went away
  with an earlier Sway, and brings them all back.
- `Detach` saves the layout and closes the windows of the focused workspace.
- `Attach` lists sessions on local and on the last remote, then restores the
  one you pick into the focused workspace.
- `List` shows the same list read-only. Pick a bound session to go to its
  workspace.
- `Kill` asks for confirmation, then kills the shells and deletes the layout.

`Resume`, `Detach` and `Attach` show i3mux's own output in a notification.
So a detach that saved nothing, because another machine took the session
or the session ended, says so.

The list lines come straight from `i3mux sessions --all`:

```
ws4  (local)  workspace 4, 2 windows
ws5  (dev-box)  workspace 5, resumable, 3 terminals backed up
ws3  (dev-box)  detached, 3 terminals saved
ws7  (dev-box)  2 shells, no saved layout
```
