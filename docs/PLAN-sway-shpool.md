# Plan: Sway-only i3mux on shpool

Date: 2026-09-24. Status: built. Reviewed twice (plan, then code). Departures are recorded in git history and the module table below is the as-built one.

## Goal

i3mux turns one Sway workspace into one "tmux window". Every terminal in the
workspace is a persistent shell (local or remote). Close the windows, come
back later, and the shells, their output, and the layout are still there.

## Findings that drive this plan

1. abduco restores nothing on reattach. Measured: 200 lines printed, detach,
   reattach emits 11 bytes (`ESC[?1049h ESC[H`). History is gone.
2. abduco switches the outer terminal to the alternate screen while
   attached. foot and alacritty do not scroll the alternate screen. That is
   the "scrolling is broken" symptom.
3. tmux restores only the visible screen on reattach (measured: last 24
   lines only) and captures keys. Not transparent.
4. shpool with `session_restore_mode = { lines = N }` replays the last N
   lines of output on reattach and never enters the alternate screen.
   Measured: all 200 lines came back. It auto-starts its daemon, takes
   `--socket` and `--config-file`, runs a command with `-c`, refuses a
   second client unless `-f`, and has `list`, `detach`, `kill`. Rust,
   Apache-2.0, maintained (v0.11.5), prebuilt musl binaries.
5. The remote lock subsystem is broken: `attach` kills the ssh lock holder
   when the command returns, so the lock is released seconds after it is
   taken. shpool's one-client rule makes the lock unnecessary.
6. The test suite cannot run here without work: no images are built, the
   i3 path needs Xvfb, and the tier/cross-WM matrix triples the surface.

## Decisions

1. Sway only. Remove i3 detection, `i3-msg`, X11 instance matching, X11
   terminals, the Xvfb image, and the cross-WM test matrix.
2. shpool replaces abduco. i3mux runs a dedicated shpool daemon per host
   with its own socket and config, so restore mode is under i3mux control.
3. No lock files. Double attach is refused by shpool; `--force` maps to
   `shpool attach -f`.
4. A workspace is bound to a *session* (name + host). The default session
   is `wsN` for workspace N, or the workspace name when it is a valid
   session name. Each terminal is a shpool session named
   `{session}-{NNN}`. Numbering comes from `shpool list` on the host, not
   from local state.
5. Closing a window never kills the shell. `i3mux terminal` reattaches the
   lowest-numbered orphan shell (alive, no window) before creating a new one.
6. `detach` saves the layout on the host that owns the shells and closes
   the windows. `attach` restores the layout and reattaches each shell.
   Works for local and remote alike.
7. `kill` kills every `{session}-*` shpool session on the host and deletes
   the layout file.
8. No files in `/tmp`. Wrapper and ssh control sockets live under
   `$XDG_RUNTIME_DIR/i3mux/`. Layout files live under the host's
   `~/.local/state/i3mux/sessions/`. shpool config under
   `~/.config/i3mux/shpool.toml` (written by i3mux when absent).
9. Remote commands run through `ssh host bash -lc '...'` so PATH is the
   login PATH. The uploaded helper script is removed.
10. The terminal title is set once at launch. No PROMPT_COMMAND injection.
11. Hosts are `enum Host { Local, Ssh(RemoteHost) }`. No `"local"` sentinel
    strings. `RemoteHost` rejects a leading `-`.
12. The shpool socket lives outside `$XDG_RUNTIME_DIR` so logind cannot
    remove it when the last ssh session ends: `~/.local/run/i3mux/shpool.socket`.
    INSTALL recommends `loginctl enable-linger` on remotes.
13. The wrapper takes argv and runs `exec "$@"`. Shell quoting happens
    exactly once, at the ssh boundary, with `shell-words`.
14. `i3mux terminal --new` skips orphan reattach. `terminal -e CMD` always
    creates a fresh session (an orphan runs a shell, not CMD).
15. `kill --session` is optional and defaults to the bound session, like
    `detach`.
16. The generated shpool config uses `session_restore_mode = { lines = 10000 }`.
    i3mux writes it only when absent and never overwrites a user edit.
17. A binding is removed only by `detach` and `kill`. A bound workspace
    with zero windows is a valid state.

## Shape

```
 sway workspace 4  ──bound to──►  session "ws4" on host H
   ├─ window (mark _i3mux:H:ws4-001) ── wrapper ── [ssh H] shpool attach ws4-001
   ├─ window (mark _i3mux:H:ws4-002) ── wrapper ── [ssh H] shpool attach ws4-002
   └─ window (mark _i3mux:H:ws4-003) ── wrapper ── [ssh H] shpool attach ws4-003

 local state (~/.config/i3mux/state.json):  {"4": {host: H, session: "ws4"}}
 host state  (~/.local/state/i3mux/sessions/ws4.json):  layout tree + name
 host daemon ($XDG_RUNTIME_DIR/i3mux/shpool.socket):    shells ws4-001..003
```

## Rules (with enforcement)

1. One workspace binds to at most one session; one (host, session) binds to
   at most one workspace on a client. Enforced: `LocalState::bind` rejects a
   duplicate (host, session) under the state flock. Test `bind_rejects_duplicate_session`.
2. A window is an i3mux window iff it carries a `_i3mux:{host}:{terminal}` mark.
   Enforced: `Mark` newtype with round-trip test.
3. Terminal names are `{session}-{NNN}`. `TerminalName::parse(session, s)`
   strips exactly `{session}-` and requires all digits, so session `ws4`
   never matches `ws4-x-001`. Test `terminal_name_prefix_is_exact`.
4. Under the local state flock: list host sessions → choose (reattach
   lowest orphan, or create max+1) → spawn → wait for window → mark.
   `choose_terminal(alive, marked, new) -> Reattach | Create` is pure and
   unit-tested. Concurrency test with 8 threads against a fake host.
5. `detach` requires ≥1 i3mux window. Enforced in handler; fixture test.
6. `attach` refuses a workspace with i3mux windows, checked under the flock
   right before binding. Attach binds *before* spawning windows, so an
   interrupted attach leaves a bound workspace that `detach` can clean.
7. `detach` writes the layout on the host atomically (`tmp` + `mv`) before
   closing windows, then closes all of them with one
   `[con_mark="^_i3mux:{host}:{session}-"] kill`. A failing host write
   leaves every window open. Test with a failing fake host.
8. `layout.rs` and `window.rs` never import `wm.rs`; they take JSON.
   Enforced by `just check` grep. Fixture trees from real `swaymsg -t get_tree`.
9. Every argv crosses one shell parse at most. `mux` builds `Vec<String>`;
   `Host::Ssh` joins once with `shell_words::join`. Test with an arg
   containing `'"$ `.
10. Double attach: shpool's refusal is caught and reported as
    "Session 'ws4-002' is already attached from another terminal or host;
    use --force to take over".
11. `attach` reports "recreated N of M shells" when the host lost sessions
    (reboot). A `shpool list` failure distinguishes ssh-down (abort) from
    daemon-not-running (empty list).

## User-facing text

- `i3mux sessions [--remote H]` prints one line per session:
  `ws4  (deepthought)  3 windows, 1 orphan` /
  `ws7  (local)  detached, 2 terminals saved`. `scripts/i3mux-rofi` is
  updated to this format in phase 5.
- `--force` help: "Disconnect any other client attached to these shells".
- Unbound workspace: "Workspace 4 is not bound to a session; run
  'i3mux activate' first".
- `activate --help` says it also opens the first terminal.
- `example-i3-config` becomes `example-sway-config`.
- INSTALL: Arch `paru -S shpool` (AUR, 0.11.5), other Linux: download the
  prebuilt musl tarball from GitHub releases or `cargo install shpool`.

## Modules

```
src/
  main.rs        CLI (clap) and command handlers only
  wm.rs          swaymsg wrapper: get_tree, get_workspaces, run
  sway_ops.rs    Sway port trait; wait-for-window-and-mark, kill_all (the only
                 code that polls or sleeps)
  window.rs      mark format, find/mark/kill command text (pure over JSON)
  launch.rs      terminal emulator argv, wrapper install
  layout.rs      Layout tree: capture (pure), restore plan (pure), sockets
  state.rs       local workspace→session bindings (unchanged store)
  host.rs        Host trait: local | ssh. run(cmd), write_file, read_file
  mux.rs         shpool commands built on Host: list, attach_cmd, kill,
                 ensure_config, next_number, orphans
  session.rs     layout file save/load on a Host
  types.rs       SessionName, RemoteHost, TerminalName, Host newtypes
  wrapper.sh     title, exec attach, pause on error
```

## Tests

- Unit: types, state, window (JSON fixtures of sway trees), layout capture
  from fixtures, restore plan from layouts, mux command building, shell
  quoting.
- Multiplexer: a pty test that runs i3mux's generated shpool attach command
  against a scratch socket: print 200 lines, detach, reattach, assert the
  first line is replayed and no alternate-screen sequence is emitted.
- Integration (containers, Sway headless + sshd): activate, terminal,
  orphan reattach, detach/attach for local and remote, layout goldens,
  scrollback visible after attach, `--force`, kill.
- `just check` runs fmt, clippy `-D warnings`, unit tests. `just check-int`
  runs the container suite.

## Phases

1. Host + mux + types + wrapper. Unit tests. shpool config generation.
2. Sway-only wm/window/layout with pure capture and restore plan.
3. main.rs command handlers on the new modules. Delete abduco/lock/helper.
4. Test infra: sway-only containers with shpool, remove tiers/dual env,
   remote tests on by default, pty multiplexer test, justfile, CI.
5. README, INSTALL, TESTING rewrite. Remove i3 example config.

## Rejected

- tmux per terminal: restores screen only, alternate screen, key capture.
- dtach: same model as abduco (no terminal state), `-r` only redraws.
- Keeping the ssh lock holder: cannot outlive the command that spawned it.
- swayipc crate instead of swaymsg: fine, but not needed for correctness;
  revisit if latency matters.
