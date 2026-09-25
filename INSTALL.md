# Installing i3mux

## Requirements

- Sway.
- A Wayland terminal: `foot` (default), `alacritty` or `kitty`. Set `TERMINAL`
  to pick one.
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

i3mux opens ssh with `ControlMaster=auto` and a control socket under
`$XDG_RUNTIME_DIR/i3mux/`, so the second and later terminals reuse the
first connection.

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
- Inspect the daemon directly:
  `shpool --socket ~/.local/run/i3mux/shpool.socket list`.
