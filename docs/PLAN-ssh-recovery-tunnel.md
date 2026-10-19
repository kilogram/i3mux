# Plan: survive ssh loss without losing the layout (tunnel variant)

Date: 2026-10-19. Status: rejected after two architecture reviews.
Chosen instead: `PLAN-ssh-recovery-controlmaster.md`.

## Decision: rejected in favour of the ControlMaster plan

This plan passed review only with many extra parts. The ControlMaster plan
gets the same recovery with less code and a more faithful shell env. Its one
real cost, sshd `MaxSessions`, has a one-line fix on hosts the user runs and
a graceful fallback elsewhere. The tunnel's in-band resize would only help on
Tailscale SSH hosts, and the tunnel cannot run there (finding 5).

| | ControlMaster | Tunnel |
|---|---|---|
| Per-host unit + landing page | yes | yes |
| Remote pieces | none new | socat relay, `setpriv`, agent link, remote `$HOME` lookup |
| ssh config handling | `ClearAllForwardings` on clients | regenerated `ssh -G` config, hidden `i3mux tunnel` |
| Tell "tunnel dropped" from "another client took the shell" | exit 255, unambiguous | `InvocationID` tracking and a test |
| Daemon recovery after reboot | existing `PRELUDE` on each attach | lock, double check, `ensure_daemon` |
| Window env | real sshd login: correct TERM, LANG, PAM env; `SSH_CONNECTION` possible via `forward_env` | built by hand; `SSH_CONNECTION` impossible |
| Invariants to enforce | few | `-D` everywhere, socat backlog, client/daemon protocol match |
| Tailscale SSH (headscale) | works, SIGWINCH issue unchanged | unsupported |
| More than about 10 windows per host | extra windows fall back to own connection; fix with `MaxSessions 100` | no limit |
| Rough size | host.rs, unit start, shell landing page | about 2-3 times that, two transports |

## Goal

When the network to a host drops, every window stays in place. Each window
shows what happened and reconnects by itself when the host is back. Only one
process per host retries the network. No window runs ssh. A window behaves
like a tmux pane in a fresh ssh login.

## Findings (tested against deepthought and headscale, 2026-10-19)

1. Today no `ServerAlive` is set, so a dead link often hangs ssh. When ssh
   does fail, every window shows the "already attached" hint, which is wrong.
2. The shpool daemon reads `/proc/<peer>/exe` and drops the connection when
   the read fails (`libshpool-0.11.5/src/daemon/peer.rs:38`). Through a plain
   `ssh -L` the peer is sshd, and the read fails with EACCES. With a socat
   relay as the peer, the read works. A different exe only logs a warning.
3. A tunnel to a remote socat relay works for `list` and `attach`. The shell's
   exit status passes through. Resize goes through the shpool protocol.
4. One tunnel carried 30 connections at the same time. sshd `MaxSessions`
   does not limit forwarded connections. The ControlMaster variant hit that
   limit at about 10 windows. socat needs `backlog=128`; its default of 5
   refused some connections.
5. Tailscale SSH refuses unix-socket forwarding: "unknown channel type".
6. The remote path in `-L` must be absolute.
7. When the tunnel dies, the client exits 1 and the shell survives as
   "disconnected". systemd restarts the tunnel in 2 s.
8. `ClearAllForwardings=yes` also clears `-L`. The output of `ssh -G HOST`
   without forward and control lines, given with `-F`, drops the user's
   `RemoteForward` and keeps the rest of the host config.
9. A shpool client without `-D` starts a local daemon when it cannot connect.
   That daemon takes over the tunnel's socket path.
10. shpool starts each shell with an empty env plus HOME, PATH, USER, SHELL,
    the forwarded variables, and `SSH_AUTH_SOCK` as a per-session link. The
    client forwards TERM, DISPLAY, LANG and SSH_AUTH_SOCK by default. On
    reattach only the agent link changes. This is how a tmux pane behaves.
11. Agent forwarding works without `-R`. The relay command links the
    tunnel's forwarded agent to a fixed path, and the client sends that path.
    The session link follows it, also after a tunnel restart.
12. `setpriv --pdeathsig TERM` stops the relay with its sshd session. After a
    real drop, the old relay stays until the remote sshd notices.
    `unlink-early` lets the new relay take the path.
13. deepthought's `~/.profile` sets `SSH_AUTH_SOCK` without a condition. This
    hides any forwarded agent, in this plan and in a plain `ssh HOST`.
14. Not tested: real packet loss (needs root) and `ssh -f` readiness together
    with a remote command.

## Decisions

1. One tunnel per remote host, run as a transient systemd user unit. The
   tunnel forwards a local socket to a socat relay in front of the remote
   shpool socket.
2. Each window runs a local `shpool attach` against the tunnel socket.
3. Other remote commands keep the existing ssh path.
4. The window wrapper becomes a landing page when the tunnel or the remote
   daemon is down.
5. Hosts that run Tailscale SSH are not supported for remote workspaces.

## Shape

```
 systemd --user: i3mux-tunnel-deepthought.service  →  i3mux tunnel deepthought
   ssh -F H.ssh_config -T -f -L $XDG_RUNTIME_DIR/i3mux/H.sock:~/.local/run/i3mux/relay.sock H
       ══ TCP ══►  sshd ── socat relay.sock ⇄ shpool.socket ── daemon
            ▲
 window 1: shpool -D -s H.sock attach ws4-001
 window 2: shpool -D -s H.sock attach ws4-002
 i3mux list / layout save / daemon start: existing ssh path
```

## Rules

Tunnel:

1. i3mux starts the unit with `systemd-run --user` when it is not active. The
   unit name escapes the host with `systemd-escape`. The unit gets the
   caller's `SSH_AUTH_SOCK` with `--setenv`.
2. The unit runs a hidden `i3mux tunnel H`. On each start it writes
   `$XDG_RUNTIME_DIR/i3mux/H.ssh_config` from `ssh -G H` without forward and
   control lines. Then it runs exec on ssh. So `Match` blocks are evaluated
   again on each restart.
3. ssh runs with `-F <that file> -T -f -S none`, `ExitOnForwardFailure=yes`,
   `StreamLocalBindUnlink=yes`, `BatchMode=yes`, `ServerAliveInterval=10`,
   `ServerAliveCountMax=3` and `ConnectTimeout=10`. Agent forwarding follows
   the user's `ForwardAgent`.
4. The remote command links `$SSH_AUTH_SOCK` to `~/.local/run/i3mux/agent.sock`,
   or removes that link when the variable is unset. Then it runs
   `exec setpriv --pdeathsig TERM socat UNIX-LISTEN:relay.sock,fork,unlink-early,backlog=128,mode=600 UNIX-CONNECT:shpool.socket`.
5. i3mux learns the remote `$HOME` once over the existing ssh path, because
   the remote side of `-L` must be absolute.
6. The unit uses `Type=forking`, `Restart=always`, `RestartSec=2s`,
   `RestartSteps=5`, `RestartMaxDelaySec=30s` and `StartLimitIntervalSec=0`.
7. A journal line that contains "unknown channel type" makes i3mux report:
   "HOST runs Tailscale SSH, which cannot forward sockets. Use OpenSSH there."
8. `detach` and `kill` stop the unit when no bound workspace uses the host.
   `detach` closes the windows before it detaches the shells, as it does
   today. Logout stops all units.

Windows:

9. One function builds every tunnel client argv. It always passes `-D`. A
   unit test checks this.
10. The client runs with `TERM=xterm-256color`, as the existing ssh path
    does. DISPLAY and LANG are removed, so the remote login sets its own
    values. `SSH_AUTH_SOCK` is the remote `agent.sock` path when the host
    forwards the agent, and is unset when it does not.
11. `SSH_CONNECTION`, `SSH_CLIENT` and `SSH_TTY` are not set. They are not
    set today either.
12. Before each attach, the window records the unit's `InvocationID`. When
    the client ends, the window runs `shpool -D list` through the tunnel.
    - The session is gone and the list works: the shell ended. Keep today's
      behaviour.
    - The session is still there and the `InvocationID` changed: the tunnel
      dropped. Reattach with `--force`, because the host may still count the
      dead client as attached.
    - The session is still there and the `InvocationID` did not change:
      another client took the shell. Show today's "taken over" hint. A
      client that loses its shell to `--force` exits 1, as after a tunnel
      drop. Only the `InvocationID` tells the two apart. A named test
      covers this.
    - The list fails and the unit is not active: show the landing page.
    - The list fails twice, 1 s apart, and the unit is active: the remote
      daemon is down. Recover as rule 13 says.
13. Daemon recovery takes the per-host lock
    `$XDG_RUNTIME_DIR/i3mux/H.recover.lock`. The window that gets the lock
    runs `mux::ensure_daemon` over the existing ssh path. The other windows
    wait for the lock, then check again. So one network process recovers
    the host. `mux::ensure_daemon` starts the daemon even when its socket
    file is gone. After recovery the window reattaches. If the shell is gone
    (reboot), the window starts a new shell in the same window and says so.
14. The landing page shows the unit's `ActiveState`, `NRestarts` and
    `RestartUSecNext`, and the last journal line. It checks again every
    second. Enter restarts the unit at once, and `q` closes the window.
15. `--force` on reconnect can take the shell back from another client that
    attached while the tunnel was down. The README states this.

Other:

16. The existing ssh path gets `ServerAliveInterval=10`,
    `ServerAliveCountMax=3` and `ConnectTimeout=10`, so commands fail and do
    not hang.
17. Local workspaces have no tunnel. Their windows use the same client argv
    against the local daemon socket, with the local env.
18. `i3mux status` adds the tunnel state of the focused workspace's host.
19. INSTALL lists socat and `setpriv` (util-linux) as remote requirements,
    OpenSSH sshd as the server, a host key accepted once with plain `ssh H`,
    and a conditional `SSH_AUTH_SOCK` line for remote profiles:
    `[ -S "$SSH_AUTH_SOCK" ] || export SSH_AUTH_SOCK=...`.

## Landing page text

```
[i3mux] Connection to deepthought lost. Shell ws4-002 is safe on the host.
[i3mux] ssh: connect to host deepthought port 22: Network is unreachable
[i3mux] Reconnecting: attempt 3, next try in 8 s.
[i3mux] Enter: retry now   q: close window
[i3mux] Log: journalctl --user -u i3mux-tunnel-deepthought
```

When the remote daemon is down, the first line reads "The shpool daemon on
deepthought is not running. Starting it."

## Phases

1. `i3mux tunnel`, the unit, the relay command, the status line. Add
   `ServerAlive` to the existing ssh path.
2. Windows use the local client through the tunnel, with the env rules.
3. Landing page and the recovery steps of rules 12 and 13.
4. README and INSTALL.

## Rejected

- Raw `ssh -L` to the shpool socket: shpool rejects sshd as the peer
  (finding 2).
- (Within this plan) ControlMaster variant: sshd `MaxSessions` caps it at
  about 10 windows per host (finding 4). The Decision section above weighs
  this again and reverses it.
- `-F /dev/null`: drops the user's host config (HostName, User, ProxyJump).
- `-R` for the agent: the forwarded agent and a link do the same job
  (finding 11).
- Exact shpool version match: shpool checks its protocol version itself.
- `i3mux relay` on the remote: needs i3mux on every host. socat is standard.
- TCP relay for Tailscale SSH hosts: any local user on the host could reach
  the shells.
- Hand-rolled daemon or autossh: systemd already supervises, restarts and logs.
- Installed unit file: needs an install step, and its options can drift from
  the binary.
- mosh or Eternal Terminal: lose scrollback or need a server on each host.
