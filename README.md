# i3mux

[![Tests](https://github.com/kilogram/i3mux/actions/workflows/test.yml/badge.svg)](https://github.com/kilogram/i3mux/actions/workflows/test.yml)

**A Sway workspace that behaves like a tmux window.**

Bind a workspace to a session, local or on a remote host over ssh. Every
terminal you open there is a persistent shell. Close the window, lose the
network, reboot the desktop: the shell keeps running, and the next terminal
you open in that workspace picks it back up with its scrollback. Detach the
workspace and attach it later, on the same machine or another, and the
window layout comes back too.

Sway does the window management. [shpool](https://github.com/shell-pool/shpool)
keeps the shells alive. i3mux glues the two together and stays out of the way:
no prefix key, no status bar, no alternate screen, native terminal scrolling.

> This project was written by AI (Claude) under human direction and review.

## Install

See [INSTALL.md](INSTALL.md). Short version: install `shpool` on every host,
build i3mux with `cargo build --release`, and add two bindings to your Sway
config:

```
bindsym $mod+Return exec i3mux terminal
bindsym $mod+m exec i3mux-rofi
for_window [con_mark="^_i3mux:"] border normal 2
```

## Use

```bash
i3mux activate                    # bind focused workspace to a local session, open a shell
i3mux activate --remote dev-box   # same, shells run on dev-box over ssh
i3mux activate --window-command 'CMD {shell}'  # each window runs CMD (see "Window commands")
i3mux terminal                    # another shell (reattaches an orphan first)
i3mux terminal --new              # always a fresh shell
i3mux terminal -e htop            # run a command in a fresh persistent shell
i3mux terminal --no-hold -e make  # close the window as soon as its shell ends
exec i3mux terminal --here        # this terminal becomes a window of the workspace's session
i3mux detach                      # save the layout, close the windows, keep the shells
i3mux attach --session ws3        # restore the layout and reattach every shell
i3mux attach --remote dev-box --session ws3 --rebind-only  # move ws3's binding to dev-box, touch no window
i3mux sessions [--remote host]    # what exists, where, and in what state
i3mux sessions --all              # this machine and every host a workspace is bound to
i3mux status                      # the focused workspace's session and ssh connection (exit 3 if none)
i3mux kill --session ws3          # kill the shells and forget the layout
i3mux open --remote dev-box ws3-004  # show an existing shell in the workspace bound to ws3
```

The default session name is `ws` plus the workspace number, or the
workspace name itself for a named workspace such as `mail`. A name with
other characters, such as `1:web`, has no default. `--session` overrides it
on `activate`, `attach` and `kill`.

## How it works

```
 Sway workspace 3  ──bound to──►  session "ws3" on dev-box
   ├─ window  _i3mux:dev-box:ws3-001  ── ssh client ── shpool attach ws3-001
   ├─ window  _i3mux:dev-box:ws3-002  ── ssh client ── shpool attach ws3-002
   └─ Firefox                         (not an i3mux window, ignored)

 systemd --user: i3mux-ssh-dev-box.service ── ssh master ══► dev-box
   (every ssh client above goes through this one connection)

 ~/.config/i3mux/state.json          which workspace is bound to which session
 dev-box:~/.local/state/i3mux/       saved layouts, one JSON file per session,
                                     and the shpool daemon log
 dev-box:~/.local/run/i3mux/         the i3mux shpool daemon socket
```

1. `activate` records the binding and opens the first terminal.
2. `terminal` asks the host's shpool daemon which shells exist. If one has no
   window, it reattaches it. Otherwise it creates the next number.
3. Each window carries a hidden Sway mark, `_i3mux:{host}:{shell}`. Marks are
   the only way i3mux recognises its windows.
4. `detach` captures the workspace tree, saves it on the host, then closes
   every marked window. The shells stay alive in shpool.
5. `attach` replays the saved tree with `swaymsg` commands and opens one
   terminal per shell. shpool restores the last 10 000 lines of each.

Closing a window never kills a shell. Only `exit` in the shell or
`i3mux kill` does. When a shell ends with an error, or within 2 s, its
window stays open with a hint until you press Enter: the shell may be
attached from another terminal. `i3mux terminal --no-hold` opens a window
that closes as soon as its shell ends or is detached, with the shell's
exit status, like tmux's `remain-on-exit off`.

## Window commands

A workspace can give each of its windows a command of your own, run
instead of i3mux's attach:

```bash
i3mux activate --window-command 'my-window --shell {shell}'
```

- i3mux splits the command into words as a shell does (quotes and
  backslashes), but runs no shell: no variables, pipes or `~`. In each
  word, `{session}` becomes the session name (`ws3`) and `{shell}` the
  name of the window's shell (`ws3-002`). Other text stays as it is.
- `terminal`, `attach` and `open` run it in every window they open in the
  workspace. The window keeps its mark, so `detach`, `kill`, `sessions`
  and `status` count it as one of the session's windows.
- i3mux makes a new shell first, without attaching it (`shpool attach
  -b`, with the `-e` command if one is given). The command attaches the
  shell itself, for example with `i3mux terminal --here --shell {shell}`
  (see "Join a session from any terminal"), which ignores the window
  command. When the command ends, the window ends: there is no hint.
- There is no `{host}`: `attach --rebind-only` can move the binding to
  another host, and `terminal --here` reads the host from the binding.
- `detach` saves the command with the layout, `attach` binds with it
  again, and `i3mux status` shows it.

## Join a session from any terminal

`i3mux terminal --here` makes the terminal it runs in a window of its
workspace's session. No window opens. Type it in the shell of any
terminal:

```bash
exec i3mux terminal --here                      # a shell of this workspace's session, here
i3mux terminal --here --remote dev-box -e htop  # bind an unbound workspace to dev-box first
i3mux terminal --here --shell ws3-002           # attach that existing shell here
```

- i3mux sets the terminal's title to a unique text and finds the Sway
  window with that title. So it finds the right window when another
  window has the focus, or when one terminal process has many windows.
  The terminal must let programs set its title (OSC 0); i3mux waits 5 s
  for the title, then gives up. Afterwards the terminal gets its own
  title back, if it has a title stack (XTWINOPS 22 and 23). i3mux sets
  no other title: the attach sets `i3mux:{host}:{shell}`, and a window
  command keeps the terminal's title.
- i3mux then marks the window as the window of a shell, and runs that
  shell's attach in place of itself (exec). It picks the shell as
  `i3mux terminal` does: an orphan, or a new one. `-e`, `--new`,
  `--force` and `--no-hold` work as they do for `terminal`. In a
  workspace with a window command, the terminal runs that command
  instead, as every window of the workspace does.
- An unbound workspace is bound first: to `--remote HOST`, or else to
  this machine, with the default session name and the
  `--window-command`, if one is given.
- i3mux refuses a workspace bound to another host than `--remote`, or
  with another window command than `--window-command` (compared word by
  word). It exits with status 1 and changes nothing, so a script can do
  something else.
- `--shell NAME` attaches the existing shell NAME of the workspace's
  session, on the binding's host, and ignores the window command. It
  never makes a shell: it refuses a shell that does not exist. Unless
  `--force` is given, it also refuses a shell that another window shows
  or another client has. With `--force`, the other window loses the
  shell's mark, so a shell never has two windows. In the window that
  already shows the shell, `--shell` needs no `--force`. If the host's
  shpool daemon was replaced, the window does not start a new shell of
  that name.
- If the attach or the window command cannot run, i3mux undoes what it
  did and exits with status 1: the window gets back the marks it had,
  the binding that i3mux made goes, and the shell that it made for the
  window command ends.
- When the shell ends or is detached, the program that ran i3mux goes on.
  Without `--no-hold`, a shell that fails or ends within 2 s shows the
  hint first. With it, the program goes on at once, with the shell's exit
  status. The window keeps its mark until it closes, so `detach` closes
  it with the other windows. With `exec`, the terminal closes when the
  shell ends.

## Move a binding to another host

`i3mux attach --remote NEW --session S --rebind-only` moves the binding
of session S, in whatever workspace has it, to host NEW (no `--remote`:
this machine). It re-marks the windows of S as windows of NEW, and does
nothing else: it opens, closes and attaches no window, contacts no host,
and starts no ssh connection. The windows keep showing what they showed.
A program that keeps a session's shells on two hosts, and moves them
itself, uses it to point the workspace at the new host; a window command
that reads the binding then follows it. Running it again is harmless: it
re-marks any window left with the old host and changes nothing else.

## Shells made by other programs

A program on a host can start a shell that i3mux shows, such as a job
that must outlive any window. This is the whole contract:

1. Make the shell in i3mux's shpool daemon on the host:
   `shpool --socket ~/.local/run/i3mux/shpool.socket --config-file
   ~/.config/i3mux/shpool.toml attach -b [-d DIR] [-c CMD] NAME`.
   shpool does not make the socket's directory: create
   `~/.local/run/i3mux` with mode 0700 first if it is missing, as i3mux
   does.
   If no daemon runs, `attach -b` starts one, and i3mux uses it from then
   on. i3mux writes the config file the first time it runs a command on
   the host; the program never writes it.
2. `NAME` is `{session}-{NNN}`. The session is the one bound to the
   workspace that should show the shell. The number is one more than the
   highest live shell of that session in `shpool list`, as `i3mux
   terminal --new` picks it.
3. `i3mux open [--remote HOST] NAME` shows the shell in the workspace
   bound to its session. It never creates a shell, and it refuses a
   session that no workspace here is bound to. Until then, the shell is an
   orphan of its session: `i3mux terminal` in that workspace reattaches
   it, and `detach`, `attach` and `kill` treat it as one of the session's
   shells.

If i3mux opens a terminal of the same session at the same moment, both
can pick the same number. shpool then attaches the second `attach -b` to
the first shell and ignores its `-c`. A program that needs its command to
run checks the shell's process afterwards.

## Remote hosts

Shells run under the remote user's login shell with the login `PATH`, so
`shpool` only needs to be on the path there. Run `loginctl enable-linger` on
the remote so shells survive the last logout.

i3mux keeps one ssh connection per remote host: an ssh master that runs as
the systemd user unit `i3mux-ssh-HOST`. Every window and every i3mux
command on that host goes through it, so a new terminal opens in tens of
milliseconds. When the network drops, the windows stay where they are.
Each one shows what happened and how systemd retries:

```
[i3mux] Connection to dev-box lost. Shell ws3-002 is safe on the host.
[i3mux] ssh: connect to host dev-box port 22: Network is unreachable
[i3mux] Reconnecting: attempt 3, next try in 8 s.
[i3mux] Enter: retry now   q: close window
[i3mux] Log: journalctl --user -u i3mux-ssh-dev-box.service
```

When the master is back, every window reattaches its shell by itself.
`i3mux status` shows the state of the connection. After each command, i3mux
stops the unit of each host that the command used, that no workspace is
bound to and that no window shows a shell of. Logout stops them all.
Without a systemd user manager, each ssh connects on its own and windows do
not reconnect. See [INSTALL.md](INSTALL.md#remote-hosts) for the ssh setup
and [docs/PLAN-ssh-recovery-controlmaster.md](docs/PLAN-ssh-recovery-controlmaster.md)
for the design.

Tailscale ssh does not deliver `SIGWINCH` in command mode, so windows will not
resize correctly through it. Use OpenSSH; see
[docs/tailscale-ssh-sigwinch.md](docs/tailscale-ssh-sigwinch.md).

## Why shpool

abduco, the previous backend, forwards bytes and keeps no terminal state.
On reattach it restores nothing, and while attached it holds the terminal on
the alternate screen, which disables scrolling in foot and alacritty. tmux
restores the visible screen only and wants a prefix key. shpool keeps a
bounded output spool and replays it on reattach without the alternate screen.
Measurements and the full design are in
[docs/PLAN-sway-shpool.md](docs/PLAN-sway-shpool.md).

## Development

```bash
just check       # fmt, clippy, unit tests, boundary check
just check-int   # container integration suite (podman or docker)
```

See [TESTING.md](TESTING.md).

## License

GPL-3.0-only. See [LICENSE.txt](LICENSE.txt).
