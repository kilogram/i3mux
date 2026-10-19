# Installing i3mux

## Requirements

- Sway.
- A Wayland terminal: `foot` (default), `alacritty` or `kitty`. Set `TERMINAL`
  to pick one.
- `bash` on the desktop. The terminal wrapper is a bash script.
- For remote hosts: OpenSSH and a systemd user session. Without systemd,
  remote hosts still work, but windows do not reconnect after a network drop.
- `shpool` on every host that runs shells: the desktop for local sessions
  and each remote host.
- Rust toolchain to build i3mux.
- Optional: `rofi` and `notify-send` for the menu script.

## Install shpool

Arch Linux (AUR):

```bash
paru -S shpool
```

Any Linux, prebuilt static binary:

```bash
curl -L https://github.com/shell-pool/shpool/releases/download/v0.11.5/shpool-x86_64-unknown-linux-musl.tar.gz \
  | tar xz
sudo install -Dm755 shpool /usr/local/bin/shpool
# or, without root:
install -Dm755 shpool ~/.local/bin/shpool
```

With cargo:

```bash
cargo install shpool
```

Check: `shpool version`.

i3mux runs its own shpool daemon per host. It never touches a shpool
daemon you run yourself. The i3mux daemon uses the socket
`~/.local/run/i3mux/shpool.socket` and the config
`~/.config/i3mux/shpool.toml`. i3mux writes that config once, with
`session_restore_mode = { lines = 10000 }`, and never overwrites it. Edit it
if you want a different scrollback size.

i3mux starts the daemon itself and appends its output, panics included, to
`~/.local/state/i3mux/shpool.log`. Over 10 MB, the log moves to
`shpool.log.1` at the next start. Starting the daemon needs `setsid` and
`flock` from util-linux, which every mainstream Linux distribution has.

## Install i3mux

```bash
git clone https://github.com/kilogram/i3mux.git
cd i3mux
cargo build --release
sudo install -Dm755 target/release/i3mux /usr/local/bin/i3mux
sudo install -Dm755 scripts/i3mux-rofi /usr/local/bin/i3mux-rofi   # optional
```

## Configure Sway

Add to `~/.config/sway/config` (see `example-sway-config`):

```
bindsym $mod+Return exec i3mux terminal
bindsym $mod+m exec i3mux-rofi
for_window [con_mark="^_i3mux:"] border normal 2
```

Reload with `$mod+Shift+c`.

## Remote hosts

1. Install `shpool` on the remote host. i3mux looks in the login-shell
   `PATH` plus `~/.local/bin` and `~/.cargo/bin`, so a user-local install
   with no root access works.
2. Use key-based ssh: `ssh-copy-id user@host`.
3. The remote login shell must be POSIX-compatible (bash, zsh, dash). i3mux
   runs `bash -lc` there, so `~/.bash_profile` and `~/.profile` must not print
   to stdout in non-interactive shells; i3mux parses that output.
4. Keep shells alive after you log out. On systemd hosts run
   `loginctl enable-linger $USER` once. Without it some distributions kill
   user processes when the last ssh session ends.
5. Accept the host key once with a plain `ssh host`. i3mux runs ssh in batch
   mode, so it never asks.
6. On hosts that you run, set `MaxSessions 100` in `/etc/ssh/sshd_config`
   and reload sshd. The default is 10 sessions per connection. Without the
   change, windows past about 10 open their own ssh connections instead of
   sharing the master's.
7. If a remote profile sets `SSH_AUTH_SOCK`, make the line conditional, so
   that it keeps a forwarded agent:
   `[ -S "$SSH_AUTH_SOCK" ] || export SSH_AUTH_SOCK=...`.

### The connection to a remote host

i3mux starts one ssh master per remote host as the transient systemd user
unit `i3mux-ssh-HOST.service`. For a host such as `me@dev-box`, systemd
escapes the name; `i3mux status` prints the exact unit. The control socket
is `$XDG_RUNTIME_DIR/i3mux/HOST.ctl`. Every window and every i3mux command on
that host is a client of the master. The master keeps your `RemoteForward`
and `LocalForward` lines, as a plain `ssh host` does. If another connection
already holds a forwarded port, ssh logs a warning, and the master still
starts. The clients forward no ports.

When the network drops, systemd restarts the master: 2 s after the first
failure, and up to 30 s apart later. Each window shows a page with the
cause and waits. When the master is back, the window reattaches its shell
with `--force`, because the host may still count the lost client as
attached. If another client attached to that shell while the connection
was down, the window takes the shell back from it.

`i3mux status` shows the state of the master. After each command, i3mux
stops the master of each host that the command used and no workspace is
bound to. So `i3mux sessions --remote HOST` or a failed `attach` leaves no
master behind. Logout stops every master.

Shells started by i3mux see `SSH_CONNECTION` and `SSH_CLIENT`, as in a
fresh ssh login. If `~/.config/i3mux/shpool.toml` on a host is older than
this feature, add this line to it:

```toml
forward_env = ["SSH_CONNECTION", "SSH_CLIENT"]
```

## Check the install

```bash
shpool version
i3mux --help
i3mux activate          # binds the focused workspace and opens a terminal
i3mux sessions
```

## Troubleshooting

- "shpool not found": install it on the host named in the error. For a
  remote, test with `ssh host bash -lc 'command -v shpool'`.
- A terminal opens and shows a shpool error, then waits for Enter: read the
  error. "already attached" means another client holds that shell. Run
  `i3mux terminal --force` or `i3mux attach --force` to take it over.
- Shells vanish after you close the ssh connection: enable linger on the
  remote (see above).
- "the shpool daemon on HOST had stopped": the daemon died, for example
  after a reboot, an OOM kill or a crash, and its shells died with it.
  i3mux has already started a new daemon, so run the command again. To find
  the cause, read `~/.local/state/i3mux/shpool.log` on that host, and
  `journalctl -k` for an OOM kill.
- Inspect the daemon directly:
  `shpool --socket ~/.local/run/i3mux/shpool.socket list`.
- "the ssh connection to HOST did not start": read the ssh error in the
  message, or the full log with
  `journalctl --user -u i3mux-ssh-HOST.service`. A host key that ssh does not
  know yet is a common cause: run a plain `ssh HOST` once.
- A window says "The connection to HOST is up, so the network did not drop":
  ssh failed for another reason, for example a changed host key. Fix the
  cause, then press Enter to retry, or `q` to close the window.
- See the connection state: `i3mux status`, or
  `systemctl --user status i3mux-ssh-HOST.service`.
