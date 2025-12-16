# Tailscale SSH SIGWINCH Issue

## Problem Summary

When using i3mux with Tailscale SSH, terminal applications (vim, tmux, abduco, etc.) do not properly resize when the terminal window is resized. This causes:
- Incorrect terminal dimensions reported by `stty size` inside multiplexer sessions
- Applications like vim showing wrong status line width
- Shell prompts (zsh RPROMPT) being misaligned
- Tools like Claude Code displaying garbled output

## Root Cause

**Tailscale SSH does not deliver SIGWINCH (window resize signal) to processes when running commands in "command mode".**

### What is Command Mode?

Command mode is when you run SSH with a command argument:
```bash
ssh host 'some-command'
```

versus interactive mode:
```bash
ssh host
# then run commands interactively
```

### The Difference

**Interactive mode (works):**
```bash
ssh s33dy
abduco -A test zsh
# Inside session: resize works, SIGWINCH is delivered ✓
```

**Command mode (fails):**
```bash
ssh -t s33dy 'abduco -A test zsh'
# SIGWINCH is NOT delivered ✗
```

i3mux launches terminals using command mode (similar to the second example), which is why SIGWINCH fails.

## Technical Details

### What Actually Happens

With Tailscale SSH in command mode:
1. Local terminal is resized
2. Tailscale SSH updates the remote PTY size (verified with `stty size`)
3. **BUT**: SIGWINCH signal is not sent to the remote process
4. Multiplexers like abduco/tmux don't know to resize their internal PTYs
5. Applications inside the multiplexer see stale dimensions

### Testing That Revealed This

**Test that PTY size updates but SIGWINCH doesn't:**
```bash
# This shows the size DOES update
ssh -t s33dy 'while true; do echo -ne "\r$(stty size)   "; sleep 0.5; done'
# ✓ Size numbers change when window is resized

# But SIGWINCH is not delivered
ssh -t s33dy 'trap "echo SIGWINCH" WINCH; while true; do sleep 1; done'
# ✗ No "SIGWINCH" message appears when resizing
```

**Same tests with OpenSSH:**
```bash
# Both work correctly with OpenSSH
ssh -t user@host 'while true; do echo -ne "\r$(stty size)   "; sleep 0.5; done'  # ✓
ssh -t user@host 'trap "echo SIGWINCH" WINCH; while true; do sleep 1; done'      # ✓
```

**Abduco-specific test:**
```bash
# Tailscale SSH command mode - abduco doesn't resize
ssh -t s33dy 'abduco -A test bash -c "while true; do stty size; sleep 1; done"'
# ✗ Size doesn't update when terminal resizes

# OpenSSH command mode - abduco resizes properly
ssh -t user@host 'abduco -A test bash -c "while true; do stty size; sleep 1; done"'
# ✓ Size updates correctly
```

## Solution: Use OpenSSH Instead

### Why This Works

Regular OpenSSH properly delivers SIGWINCH in both interactive and command modes.

### Setup Instructions

**1. Configure firewall to allow OpenSSH only from Tailscale network:**

```bash
# Allow SSH only from Tailscale IPs (100.64.0.0/10)
sudo firewall-cmd --permanent --add-rich-rule='rule family="ipv4" source address="100.64.0.0/10" port port="22" protocol="tcp" accept'

# Remove SSH from public zone if present
sudo firewall-cmd --permanent --remove-service=ssh 2>/dev/null || true

# Apply changes
sudo firewall-cmd --reload

# Verify
sudo firewall-cmd --list-all
```

**2. Get Tailscale IP of remote host:**

```bash
tailscale status | grep hostname
```

**3. Configure SSH client (`~/.ssh/config`):**

```
Host remote-host
    HostName 100.x.y.z  # Tailscale IP from step 2
    User yourusername
```

**4. Test it works:**

```bash
# Test SIGWINCH delivery
ssh remote-host 'trap "echo SIGWINCH" WINCH; while true; do sleep 1; done'
# Should print "SIGWINCH" when you resize the terminal

# Test abduco resize
ssh remote-host 'abduco -A test bash -c "while true; do stty size; sleep 1; done"'
# Size should update when you resize the terminal
```

### Security Note

This setup maintains security because:
- OpenSSH only listens on the Tailscale network (100.64.0.0/10)
- Not exposed to public internet
- Only devices in your Tailscale network can connect
- You can still use Tailscale's ACLs for additional access control

### Alternative: Different Port

If you want to keep Tailscale SSH available on port 22, run OpenSSH on a different port:

```bash
# /etc/ssh/sshd_config
Port 2222

# Firewall
sudo firewall-cmd --permanent --add-rich-rule='rule family="ipv4" source address="100.64.0.0/10" port port="2222" protocol="tcp" accept'
sudo firewall-cmd --reload

# SSH config
Host remote-host
    HostName 100.x.y.z
    Port 2222
    User yourusername
```

## Status of Tailscale Bug Report

As of 2025-12-15, this issue has not been reported to Tailscale. A search of their GitHub issues found:
- No issues mentioning SIGWINCH
- No issues about terminal resize in command mode
- Related issues were about the web console (ssh-wasm), not the SSH protocol

This is a legitimate unreported bug in Tailscale SSH's PTY handling.

## References

- Tailscale SSH documentation: https://tailscale.com/kb/1193/tailscale-ssh/
- SIGWINCH signal: https://man7.org/linux/man-pages/man7/signal.7.html
- Similar issue with abduco over regular SSH: https://github.com/martanne/abduco/issues/15
