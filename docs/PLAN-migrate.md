# Plan: migrate a session to another host

Date: 2026-10-23. Status: draft 2, after architecture and design review.
kg chose the feature. Open: whether `-e` stays (see Open).

## Goal

`i3mux attach --session S --remote NEW --migrate` moves session S to host
NEW while its windows stay open. Afterwards S behaves as if it had always
been on NEW: `detach`, `attach`, `sessions`, `status` and `kill` find it
there. Processes do not move. Each shell that NEW lacks starts again there
as a login shell in the home directory.

It replaces `attach --rebind-only`, which moved only the binding.

## Decisions

1. **One command under one lock.** `--migrate` holds the state lock from
   its first check to its end, as every other command does.
2. **Refusals first.** Before it changes anything, i3mux refuses:
   - a NEW that has a saved layout of S, or a binding of S in another
     workspace (it would overwrite a different session);
   - a session with a pending cleanup of a third host, neither NEW nor
     the host it is bound to;
   - a host that does not answer.

   A migration to the host of a pending cleanup cancels that cleanup. It
   keeps that host's shells (decision 4). The new pending cleanup names
   the host that the session leaves.
3. **It says what will happen.** One line on stderr first, for example
   `i3mux: moving ws3 from s0nny-box to dev-box: 1 of 2 shells starts
   again there as a login shell in ~; programs running on s0nny-box end.`
4. **Shells on NEW.** For each shell N of S on the old host, i3mux keeps
   an N that NEW already has. It makes a missing N as a login shell in the
   home directory (`shpool attach -b N`). Then it checks that every N
   exists on NEW. A failure ends the shells that this command made on NEW,
   and nothing else changes.
5. **Commit.** i3mux moves the binding to NEW and records the old host as
   a pending cleanup, in one state write. Then it re-marks the windows.
6. **Cleanup.** i3mux copies the saved layout of S, if the old host has
   one, to NEW. Then it ends the shells of S on the old host, deletes the
   old layout, and clears the pending cleanup.
7. **A rerun finishes.** If S is on NEW with a pending cleanup, a migration
   to NEW re-marks the windows and runs step 6. If S is on NEW with no
   cleanup, it prints `ws3 is already on dev-box` and exits 0.
8. **A pending cleanup is visible and is never lost.** `sessions` and
   `status` show it, with the command that finishes it. `detach` and
   `kill` run the cleanup first. If the old host does not answer, they
   refuse and say so.
9. **Windows follow.** The wrapper runs `i3mux reattach --shell N --from
   HOST`, a hidden command, before every run of its attach after the
   first. That is after every end, for any exit status, and again after
   ssh recovery, before REATTACH. It reads the binding, not the window,
   so it needs no title. It never execs:
   - Bound to another host that has N: it prints `[i3mux] ws3 moved to
     dev-box. Attaching ws3-001.` and the new wrapper argv, and exits 0.
     The wrapper execs that argv.
   - Bound to another host that does not answer: a distinct status. The
     wrapper waits for that host, and never attaches on its old host.
   - Otherwise: a third status. The wrapper takes today's path (ssh
     recovery, a replaced daemon, the hint, or `--no-hold`).
10. **Window commands are their own.** Decision 9 does not change a window
    that runs a window command. That command decides, as before.

## Rules

1. Before step 5, only new shells on NEW change, and a failure ends them.
2. After step 5, every window of S is marked with NEW, and every shell of S
   existed on NEW at the check.
3. After step 6, no shell of S exists on the old host.
4. A migration closes no window, unless a shell on NEW ended.

## Picture

```
 ws3 bound to A: windows 001 002 ─► A:ws3-001 A:ws3-002   (lock held throughout)
 2  refuse if B has ws3's layout or binding
 4  B has ws3-001? keep. ws3-002 missing? make it. check both exist.
 5  ws3 → B, cleanup A pending (one write); re-mark windows
 6  copy layout A→B; end A:ws3-*; delete A's layout; clear pending
    each wrapper's attach ends → i3mux reattach → attaches B:ws3-00N
```

## Enforcement

- Rules 1 to 3: a test cuts the command at each step boundary and checks
  that a rerun reaches the same end state.
- The pending cleanup is its own field of the binding, a sum type, not a
  flag.
- Rule 3 in the wrapper: a test with a fake ssh that exits 75, then 255,
  after a migration. `reattach` runs first each time, and the old host
  gets no attach.
- Rule 4: a container test migrates a session with two windows, one with a
  running job, and checks that both windows show NEW's shells.

## README changes

- The Use line for `--rebind-only` becomes `--migrate`.
- "Move a binding to another host" becomes "Move a session to another
  host", with what moves and what does not.
- The Window commands bullet that names `--rebind-only`.
- A line under "Closing a window never kills a shell": a window follows
  its session to a new host.
- The exit status table, for the reattach statuses.
- The README does not name handoff.

## Open

- **`-e CMD`** (kg proposed it). A program that wants its own shells on NEW
  can make them first, and `--migrate` keeps them (decision 4). If `-e`
  comes back, it runs CMD once per shell, with `{session}` and `{shell}`,
  before the check of decision 4.

## Later (TODO)

- **Shell command per binding**, like tmux's `default-command` (kg,
  2026-10-23). Every shell that i3mux makes for the session runs it, with
  `{session}` and `{shell}`.

## Rejected

- Keep `--rebind-only` beside `--migrate`: windows then have nothing to
  attach.
- Follow only when a shell ends with status 0: the replaced-daemon and ssh
  paths would remake the shell on the old host first.
- Reuse `terminal --here --shell N` to follow: it finds its window by a
  title, and alacritty locks the title that i3mux gives it.
- Release the lock while shells are made: a new shell on the old host
  would then end with no counterpart.
