# Plan: keep the layout when Sway dies

Date: 2026-10-19. Status: approved.

## Goal

A bound workspace keeps its layout when Sway dies without `detach`: battery
death, a crash, or logout. The next `i3mux attach` or `i3mux terminal` in
that workspace brings all its windows back. The only network step is one
check at restore. When two machines used the same session, the machine that
took it last wins, and the other machine says so.

## Findings

1. Today only `detach` saves the layout, on the host. After a crash the
   binding in `state.json` survives, but the workspace has no windows.
2. Suspend does not lose the layout. Sway and its windows survive, and the
   ssh landing page reconnects them.
3. `attach`, `detach` and `kill` hold the local state lock while they change
   windows and bindings.
4. `swaymsg -t subscribe -m` prints one JSON line per Sway event. i3mux
   already depends on `swaymsg`.
5. Sway closes windows one by one after a `kill` command returns. A capture
   during that time sees a part of the layout.
6. An unparseable `state.json` is quarantined, and that drops all bindings
   (`state.rs`).

## Decisions

1. Each session on the host has a token: 16 random bytes. A take writes a
   new token. Tokens are compared for equality only. No clocks, no counters.
2. A binding records the token it took and its owner: the Sway instance
   (boot id and `$SWAYSOCK`) that holds its windows.
3. A watcher keeps a local backup of each owned binding's layout, from Sway
   events.
4. A binding whose owner is not the current Sway is resumable. `attach` and
   `terminal` resume it.

## Shape

```
 machine A                                     host: ~/.local/state/i3mux/sessions/
 sway ──events──► i3mux watch (state lock)       ws4.json   layout from detach
                    │ owner == this sway?      host: $XDG_RUNTIME_DIR/i3mux/sessions/
                    │                            ws4.token  T2
                    ▼
 ~/.local/state/i3mux/backups/HOST/ws4.json  {token T1, layout}
 state.json: ws 4 → {host, ws4, token T1, owner}

 resume in ws 4:  host token == T1 → restore the backup
                  host token != T1 → drop the binding, attach from ws4.json
                  no token file    → session ended: drop the binding, attach
                  host unreachable → error, nothing changes
```

## Rules

Token:

1. A take writes a new token to `SESSION.token` on the host under `flock`.
   `activate`, `attach` from the host layout, and `kill` take.
1a. The token file lives in `$XDG_RUNTIME_DIR/i3mux/sessions/` on the host,
   or in `~/.local/state/i3mux/sessions/` when that variable is unset. A host
   reboot removes it, so the session ends, as with tmux.
1b. `kill` deletes the token file under the same `flock`. A missing token
   file means the session ended.
2. `attach` takes under the local state lock, after all its refusals.
3. A resume reads the token and never writes it.
4. `detach` writes the host layout only when the host token equals the
   binding's. The compare and the write run in one host script under
   `flock`. A binding with no token matches a host with no token file.
5. When `detach` finds a different token, it closes its windows and unbinds.
   It does not run `shpool detach` and does not write the host layout. It
   says that another machine took the session, or, with no token file, that
   the session ended.

Backup:

6. The watcher writes a backup only for a binding owned by its own Sway, with
   at least one i3mux window. It writes under the state lock.
7. The watcher captures the current Sway tree 1 s after the first event of a
   burst. It never uses the event contents.
8. On the Sway `shutdown` event, the watcher exits without writing.
9. A backup file holds the token of its binding. A backup with another
   token is ignored.
10. The watcher never contacts a host.

Resume:

11. `attach` or `terminal` in a workspace with a resumable binding resumes
    it. It reads the host token first.
12. Same token: restore the backup and reattach each shell. Set the owner
    only after the last window is marked. An interrupted resume then leaves
    the backup unchanged.
13. Different token: drop the binding and the backup. Say "ws4 was taken by
    another machine since; using the layout saved there." Then attach from
    the host layout, as today.
13a. No token file: drop the binding and the backup. Say "ws4 ended on
    HOST." Then attach from the host layout, as today.
14. Unreachable host or unreadable token: report the error. The binding and
    the backup do not change.
15. No backup: attach from the host layout, as today.
16. `detach` and `kill` remove the binding and its backup.

Other:

17. The watcher runs as the transient user unit `i3mux-watch-<sway pid>`.
    i3mux starts it when it binds a workspace. It gets `SWAYSOCK` and the
    state path with `--setenv`. It uses `Restart=on-failure` and exits 0
    when Sway closes its socket.
18. A master stays up only for hosts of bindings owned by the current Sway.
19. New `state.json` fields use serde defaults. Today's file loads unchanged.
20. Without a systemd user manager there is no watcher and no backup.
21. The guarantee is "the last take wins". The ssh landing page reattaches
    with `--force` and does not take. INSTALL says this.
22. The host scripts need `flock` (util-linux). INSTALL says so.

## Phases

1. Tokens on the host: take, compare-and-write, `kill`, and `detach`
   (rules 1-5).
2. Owner, backup files and resume in `attach` and `terminal` (rules 9,
   11-16, 18-19).
3. The watcher and its unit (rules 6-8, 10, 17, 20).
4. `sessions` text, README and INSTALL.

## Rejected

- Clocks or counters to order layouts: clocks differ, and a counter can
  repeat after `kill`. Equality on a random token is enough.
- Token on disk with the host boot id: works without linger, but adds a boot
  id check. Cost of the runtime dir: without linger, logind can remove it
  while shells live. i3mux then says "ended" and uses the host layout.
- `live`/`closed` modes set at watcher start: wrong after a watcher restart
  or a window opened before the watcher. The owner field replaces them.
- Backup inside `state.json`: one bad layout would quarantine every binding.
- No watcher, capture at i3mux commands only: loses the arrangement, which
  is what a crash destroys.
- Pushing backups to the host: a second write path. `detach` writes there.
- Merging two layouts: a layout is one value.
- `swayipc` crate: `swaymsg` gives the same events.
- i3-resurrect, `i3-save-tree`: i3 only. Sway has no `append_layout`.
- `q` on the landing page closes all session windows: needs i3mux inside the
  wrapper and changes ssh-recovery rule 11. Separate follow-up.
- Automatic restore at login: opens windows for hosts that may be down.
