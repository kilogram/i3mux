# Plan: a terminal joins a session, window commands, rebind

Date: 2026-10-21. Status: chosen.

## Goal

Five small features that let a program build on i3mux's sessions without
opening its own windows:

1. `i3mux terminal --here`: the terminal it runs in becomes a window of a
   session.
2. A window command per binding: each window of the workspace runs it
   instead of i3mux's wrapper.
3. `i3mux attach --rebind-only`: move a binding to another host. Nothing
   opens, closes or attaches.
4. `--no-hold`: a window ends when its shell ends, with no pause.
5. A host keeps its ssh master while a window is marked with it.

## Findings

1. A program can find its own window by a title it sets. It writes OSC 0
   (`ESC ] 0 ; TITLE BEL`) to `/dev/tty` and looks for a Sway node whose
   `name` is that title. Tested in a headless Sway: alacritty 0.17 (one
   process per window, and three windows of one process through `alacritty
   msg create-window`), foot and footclient (two windows of one `foot
   --server`). Each window showed its own title within 0.2 s. The pid
   cannot tell windows of one process apart, and the focused window can be
   another one.
2. A program that an interactive shell runs can `exec` the wrapper. The
   wrapper then owns the terminal: its process group is the foreground
   group, it reads what the user types, and when it ends the shell prompts
   again with its exit status (tested on a pty with bash).
3. `shpool attach -b -c CMD NAME` on an existing NAME attaches that shell and
   ignores CMD. So `--here --shell NAME` never creates a shell.

## Decisions

1. **`terminal --here`.** It sets a nonce title, finds the node with that
   title, and takes the workspace from the tree. Then, under the state
   lock: it binds an unbound workspace, picks the shell, and marks this
   window (its old i3mux mark goes). Last, after the masters of unused hosts
   stop, it `exec`s the wrapper, or the window command.
   - `--remote HOST` names the host. Without it, the host is the binding's,
     or this machine for an unbound workspace.
   - A workspace bound to another host than `--remote` is refused before
     anything changes: exit 1, and the message names both hosts. So is a
     `--window-command` other than the binding's.
   - `--shell NAME` attaches NAME, which must exist and belong to the
     binding's session. It ignores the window command, and the wrapper does
     not start a new shell when the daemon was replaced (exit 75).
   - A terminal whose title does not change within 5 s is refused: the
     emulator must let programs set the title.
2. **Window command.** A string, split into words like a shell does, with no
   shell. In each word `{session}` becomes the session name and `{shell}`
   the shell name. No `{host}`: a rebind changes the host, so a window that
   needs it reads the binding (`i3mux terminal --here --shell {shell}` does).
   It is part of the binding (`activate --window-command`, `terminal --here
   --window-command`). `detach` saves it with the layout and `attach`
   restores it. `terminal`, `attach` and `open` run it in every window they
   open. i3mux makes a missing shell first (`shpool attach -b`), so the
   command can attach it. The window keeps its mark.
3. **Rebind only** (`attach --remote NEW --session S --rebind-only`). It
   moves the one binding of session S to NEW and re-marks the windows of S
   that show another host. It contacts no host and starts no master. A
   second run repeats the re-mark and changes nothing else.
4. **`--no-hold`** on `terminal`: the wrapper exits with the shell's status
   when the shell ends or is detached, with no "Press Enter". The landing
   page of a lost connection still shows.
5. **Masters.** At the end of each command, the master of a touched host
   stops only when no binding names the host and no window is marked with
   it.

## Rejected

- Finding the window by pid: one terminal process can own many windows.
- Finding it as the focused window: the user may focus another first.
- `{host}` in window commands: stale after a rebind.
- `--no-hold` per binding: the caller of one command knows best.
