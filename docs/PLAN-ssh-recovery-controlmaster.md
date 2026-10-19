# Plan: survive ssh loss without losing the layout (ControlMaster)

Date: 2026-10-19. Status: chosen, approved by architecture review.
Rejected alternative: `PLAN-ssh-recovery-tunnel.md` (see its "Decision"
section for the comparison).

## Goal

When the network to a host drops, every window stays in place. Each window
shows what happened and reconnects by itself when the host is back. Only one
process per host retries the network. A window behaves like a tmux pane in a
fresh ssh login.

## Findings (tested against deepthought, 2026-10-19)

1. Today no `ServerAlive` is set, so a dead link often hangs ssh. When ssh does
   fail, every window exits 255 and shows the "already attached" hint, which
   is wrong for this case.
2. A raw `ssh -L` tunnel to the shpool socket cannot work. The shpool daemon
   reads `/proc/<peer>/exe` and drops the connection when this fails
   (`libshpool-0.11.5/src/daemon/peer.rs:38`). Through a tunnel the peer is
   sshd, which is not dumpable, so the read fails with EACCES.
3. The user's `RemoteForward 5555` collides with the forward of an existing
   connection. With `ExitOnForwardFailure=yes` this kills the new ssh.
   Without it, the collision is only a warning.
4. A master started as `ssh -M -S CTL -f -N` under systemd `Type=forking` is
   "active" only after it authenticates and its control socket exists.
5. When the master dies, attached clients exit 255 at once. The remote shell
   survives as "disconnected". systemd restarts the master in 2 s.
6. sshd `MaxSessions` (default 10) limits sessions per connection. Of 12
   concurrent clients on one master, 2 were refused. With `ControlMaster=no`
   and no guard, both fell back to a direct connection and finished.
7. The window runs inside a real sshd login, through `bash -lc`. TERM, LANG
   and the forwarded agent are the same as in a fresh `ssh HOST`. shpool
   starts the shell with an empty env plus HOME, PATH, USER, SHELL, the
   forwarded variables and an agent link. On reattach only the agent link
   changes. This is how a tmux pane behaves.
8. deepthought's `~/.profile` sets `SSH_AUTH_SOCK` without a condition. This
   hides any forwarded agent, in this plan and in a plain `ssh HOST`.
9. Not tested: real packet loss. That needs root. `ServerAlive` is standard
   ssh behaviour.

## Decisions

1. One ssh master per remote host, run as a transient systemd user unit.
2. Every remote ssh from i3mux is a client of that master. This covers
   windows and `run` commands alike.
3. The window wrapper becomes a landing page after a transport failure.

## Shape

```
 systemd --user
   i3mux-ssh-deepthought.service ── ssh -M -N ══ TCP ══► deepthought sshd
          control socket: $XDG_RUNTIME_DIR/i3mux/deepthought.ctl
              ▲                ▲                     ▲
   window: ssh -S ctl -t    window: ssh -S ctl -t    i3mux: ssh -S ctl
           shpool attach            shpool attach           shpool list
```

## Rules

1. i3mux starts the unit with `systemd-run --user` before each remote
   command, if it is not active. The unit name escapes the host with
   `systemd-escape`. The unit gets the caller's `SSH_AUTH_SOCK` with
   `--setenv`. When two commands start it at the same time, the "unit
   already exists" error counts as success.
2. The unit runs `ssh -M -S CTL -f -N` with `ControlPersist=no`,
   `ServerAliveInterval=10`, `ServerAliveCountMax=3`, `ConnectTimeout=10`,
   `BatchMode=yes` and `ExitOnForwardFailure=no`. The master keeps the
   user's forwards, as a fresh ssh does. A forward collision is only a
   warning.
3. The unit uses `Type=forking`, `Restart=always`, `RestartSec=2s`,
   `RestartSteps=5`, `RestartMaxDelaySec=30s` and `StartLimitIntervalSec=0`.
4. Every client uses `-S CTL -o ControlMaster=no -o ClearAllForwardings=yes`,
   plus the `ServerAlive`, `ConnectTimeout` and `BatchMode` options of rule 2.
   One function builds this argv, and a unit test checks it.
5. A client that the master refuses (`MaxSessions`) falls back to its own
   direct connection. It never retries by itself.
6. A non-window command fails at once when the unit does not start. The
   error names the unit and the `journalctl` command.
7. After exit 255, the window shows the landing page. It reads the unit's
   `ActiveState`, `NRestarts` and `RestartUSecNext`, and the last journal line.
8. Before each attach, the window records the unit's `InvocationID`. After
   exit 255, the landing page waits until the unit is active with a new
   `InvocationID`. Then it reattaches with `--force`, because the host may
   still count the dead client as attached. If the `InvocationID` did not
   change, the cause is not a dropped link (for example a changed host key).
   The window then shows ssh's error and waits for Enter. A wrapper test
   covers "exit 255, unit active, same `InvocationID`".
9. After exit 75 (the remote daemon was replaced, for example after a
   reboot), the window reattaches once. This starts a new shell in the same
   window. Only the first window gets exit 75 and says so. The other windows
   find the new daemon and start a new shell without a message.
10. Any other exit status keeps today's hint and the wait for Enter. A
    client that loses its shell to `--force` exits 1, never 255. So two
    hosts that take a shell from each other do not start a loop.
11. On the landing page, Enter restarts the unit at once, and `q` closes
    the window.
12. After each command, i3mux stops the unit of each host that the command
    used and that no bound workspace uses. Logout stops all units.
13. `i3mux status` adds the connection state of the focused workspace's
    host.
14. The local host has no unit and no landing page. Nothing changes for it.
15. The existing `ControlMaster=auto` and `ControlPersist=10m` options in
    `host.rs` are removed.
16. The generated `shpool.toml` gets `forward_env = ["SSH_CONNECTION",
    "SSH_CLIENT"]`, so new shells see them. i3mux writes this file only when
    it is absent. Existing installs add the line by hand.
17. INSTALL says:
    - Accept the host key once with plain `ssh HOST`, because the unit runs
      in batch mode.
    - Set `MaxSessions 100` in sshd on hosts you run. Without it, windows
      past about 10 open their own connections.
    - Make the `SSH_AUTH_SOCK` line in remote profiles conditional:
      `[ -S "$SSH_AUTH_SOCK" ] || export SSH_AUTH_SOCK=...`.
    - `--force` on reconnect can take a shell back from another client that
      attached while the connection was down.

## Landing page text

```
[i3mux] Connection to deepthought lost. Shell ws4-002 is safe on the host.
[i3mux] ssh: connect to host deepthought port 22: Network is unreachable
[i3mux] Reconnecting: attempt 3, next try in 8 s.
[i3mux] Enter: retry now   q: close window
[i3mux] Log: journalctl --user -u i3mux-ssh-deepthought
```

## Phases

1. Master unit and client options in `host.rs`. Start-on-demand. Status line.
2. Landing page and exit-75 reattach in the wrapper.
3. README and INSTALL.

## Rejected

- Tunnel to a remote socat relay (`PLAN-ssh-recovery-tunnel.md`): about 2-3
  times the code, a hand-built env, and no Tailscale SSH support.
- `ssh -L` tunnel straight to the shpool socket: shpool rejects sshd as the
  peer (finding 2).
- Retry inside each window with `ControlMaster=auto`: each window races to
  open its own connection.
- `ProxyCommand=false` guard on clients: a client refused by `MaxSessions`
  then loops on the landing page (finding 6).
- `ClearAllForwardings=yes` on the master: drops forwards that a fresh ssh
  gives.
- Hand-rolled daemon or autossh: systemd already supervises, restarts and logs.
- Installed unit file: needs an install step, and its options can drift from
  the binary.
- mosh or Eternal Terminal: lose scrollback or need a server on each host.
