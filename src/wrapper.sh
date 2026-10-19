#!/usr/bin/env bash
# i3mux terminal wrapper. i3mux builds its command line in launch.rs:
#
#   wrapper.sh TITLE ARGV...
#   wrapper.sh --remote HOST SHELL UNIT N REATTACH... TITLE ARGV...
#
# Sets the window title once and runs ARGV as given (no shell parses it).
# Keeps the window open with a hint when ARGV fails or ends within 2 s.
# shpool refuses a second client of a shell with exit status 0, so a quick
# end is the only sign that another terminal or host took the shell first.
#
# A remote window (--remote) attaches SHELL on HOST through the ssh master
# that the systemd user unit UNIT runs. REATTACH is the N-word argv that
# attaches SHELL again with --force. It recovers as
# docs/PLAN-ssh-recovery-controlmaster.md rules 7-11 say:
#
#   exit 255  ssh lost its connection. Show the landing page until UNIT is
#             active with a new InvocationID, then run REATTACH: the host
#             may still count the lost client as attached. If the
#             InvocationID did not change, the master never dropped, so ssh
#             failed for another reason. Show ssh's error and wait for a key.
#   exit 75   The remote shpool daemon was replaced. Run ARGV once more,
#             which starts a new shell in this window.
#
# bash, not sh: the landing page refreshes once a second while it waits for
# a key, which needs `read -t`, and REATTACH needs an array.
set -u

remote=
if [ "${1-}" = --remote ]; then
    remote=1 host=$2 shell=$3 unit=$4
    reattach=("${@:6:$5}")
    shift $((5 + $5))
    printf -v unit_shown '%q' "$unit"
fi
title=$1
shift
argv=("$@")

say() { printf '[i3mux] %s\n' "$*"; }
clear_screen() { printf '\033[H\033[2J'; }

# Read the properties of UNIT into active, sub, inv, restarts, next_delay
# and changed (a unix time).
read_unit() {
    active= sub= inv= restarts=0 next_delay= changed=
    local key value
    while IFS='=' read -r key value; do
        case $key in
            ActiveState) active=$value ;;
            SubState) sub=$value ;;
            InvocationID) inv=$value ;;
            NRestarts) restarts=$value ;;
            RestartUSecNext) next_delay=$value ;;
            StateChangeTimestamp) changed=${value#@} ;;
        esac
    done < <(systemctl --user show --timestamp=unix -p ActiveState -p SubState \
        -p InvocationID -p NRestarts -p RestartUSecNext -p StateChangeTimestamp \
        "$unit" 2>/dev/null)
}

# The reconnect line of the landing page. attempts counts the restarts
# since this window attached; a manual restart resets systemd's counter.
progress() {
    local attempts=$((restarts - seen_restarts)) delay left
    [ "$attempts" -lt 0 ] && attempts=0
    case $active/$sub in
        activating/auto-restart)
            # RestartUSecNext is the pending wait, for example "5.908353s".
            delay=${next_delay%s}
            delay=${delay%%.*}
            if [[ $delay =~ ^[0-9]+$ && $changed =~ ^[0-9]+$ ]]; then
                left=$((changed + delay - $(date +%s)))
                [ "$left" -lt 0 ] && left=0
                echo "Reconnecting: attempt $((attempts + 1)), next try in $left s."
            else
                echo "Reconnecting: attempt $((attempts + 1))."
            fi ;;
        activating/*) echo "Reconnecting: attempt $((attempts > 0 ? attempts : 1)), connecting now." ;;
        *) echo "Not reconnecting: the connection unit is ${active:-unknown}. Press Enter to start it." ;;
    esac
}

landing_page() {
    local line
    say "Connection to $host lost. Shell $shell is safe on the host."
    line=$(journalctl --user -u "$unit" -t ssh -n 1 -o cat --no-pager 2>/dev/null)
    [ -n "$line" ] && say "$line"
    say "$(progress)"
    say "Enter: retry now   q: close window"
    say "Log: journalctl --user -u $unit_shown"
}

error_page() {
    say "ssh to $host failed with exit status 255. Shell $shell is safe on the host."
    say "The connection to $host is up, so the network did not drop."
    if [ -n "$ssh_error" ]; then
        say "${ssh_error##*$'\n'}"
    else
        say "ssh printed no error. See the log below."
    fi
    say "Enter: retry   q: close window"
    say "Log: journalctl --user -u $unit_shown"
}

# After exit 255: wait until the master is back, refreshing once a second.
# Returns 0 to reattach and 1 to close the window.
recover() {
    local key restart_error=
    while :; do
        read_unit
        if [ "$active" = active ] && [ "$inv" != "$seen_inv" ]; then
            return 0
        fi
        clear_screen
        if [ "$active" = active ]; then error_page; else landing_page; fi
        [ -n "$restart_error" ] && say "$restart_error"
        if IFS= read -rsn1 -t 1 key; then
            case $key in
                q | Q) return 1 ;;
                '')
                    [ "$active" = active ] && return 0
                    restart_error=$(systemctl --user restart --no-block "$unit" 2>&1) ;;
            esac
        elif [ $? -le 128 ]; then
            return 1 # end of input: the terminal is gone
        fi
    done
}

printf '\033]0;%s\007' "$title"
replaced=
while :; do
    start=$(date +%s)
    if [ -z "$remote" ]; then
        "${argv[@]}"
        status=$?
        break
    fi
    read_unit
    seen_inv=$inv seen_restarts=$restarts
    # ssh's own messages go to stderr; keep them for the error page. The
    # remote shell's output reaches the terminal through the pty on stdout.
    { ssh_error=$("${argv[@]}" 2>&1 >&3 3>&-); } 3>&1
    status=$?
    if [ "$status" -eq 255 ]; then
        recover || exit "$status"
        argv=("${reattach[@]}")
        clear_screen
        continue
    fi
    [ -n "$ssh_error" ] && printf '%s\n' "$ssh_error" >&2
    if [ "$status" -eq 75 ] && [ -z "$replaced" ]; then
        replaced=1
        say "Starting a new shell in this window."
        continue
    fi
    break
done
elapsed=$(($(date +%s) - start))
if [ "$status" -ne 0 ] || [ "$elapsed" -lt 2 ]; then
    printf '\n[i3mux] %s ended after %s s with exit status %s.\n' "$title" "$elapsed" "$status"
    printf '[i3mux] If the shell is already attached from another terminal or host,\n'
    printf "[i3mux] run 'i3mux terminal --force' or 'i3mux attach --force' to take it over.\n"
    printf '[i3mux] Press Enter to close this window.\n'
    read -r _ || true
fi
exit "$status"
