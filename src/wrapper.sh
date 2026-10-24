#!/usr/bin/env bash
# i3mux terminal wrapper. i3mux builds its command line in launch.rs:
#
#   wrapper.sh [--no-hold] [--attach-only] I3MUX HOST SHELL TITLE ARGV...
#   wrapper.sh [--no-hold] [--attach-only] I3MUX HOST SHELL --remote UNIT N REATTACH... TITLE ARGV...
#
# Sets the window title once and runs ARGV as given (no shell parses it).
# ARGV attaches shell SHELL on HOST (`local`, or a remote host).
# Keeps the window open with a hint when ARGV fails or ends within 2 s.
# shpool refuses a second client of a shell with exit status 0, so a quick
# end is the only sign that another terminal or host took the shell first.
# With --no-hold it never keeps the window: it exits with ARGV's status.
# With --attach-only, ARGV only attaches an existing shell: after exit 75
# it does not run ARGV again, which would start a new shell of that name.
#
# The window follows its session to another host (docs/PLAN-migrate.md
# decision 9). Before every rerun of its attach, and after the last end,
# whatever the exit status, it runs `I3MUX reattach --shell SHELL --from
# HOST`, which never execs:
#
#   exit 0    The session is bound to another host. reattach printed its
#             line on stderr and the wrapper argv for that host, as
#             NUL-terminated words, on stdout. Exec that argv. If that host
#             does not answer, the new wrapper's ssh recovery waits for it.
#   other     The window stays on HOST: go on as below.
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
# It never runs ARGV on a terminal with 0 rows or 0 columns, or on one that
# is gone (a character device that is no terminal any more): shpool panics
# on an attach at that size, and the shell can never be attached again
# (shell-pool/shpool#445). It exits instead, after one line. It sets no
# SIGHUP trap: a hung-up window ends it at once, also while ARGV runs.
#
# bash, not sh: the landing page refreshes once a second while it waits for
# a key, which needs `read -t`, and REATTACH needs an array.
set -u

no_hold= attach_only= options=()
while :; do
    case ${1-} in
        --no-hold) no_hold=1 ;;
        --attach-only) attach_only=1 ;;
        *) break ;;
    esac
    options+=("$1")
    shift
done

i3mux=$1 host=$2 shell=$3
shift 3
remote=
if [ "${1-}" = --remote ]; then
    remote=1 unit=$2
    reattach=("${@:4:$3}")
    shift $((3 + $3))
    printf -v unit_shown '%q' "$unit"
fi
title=$1
shift
argv=("$@")

say() { printf '[i3mux] %s\n' "$*"; }
clear_screen() { printf '\033[H\033[2J'; }

# Ask i3mux where the session of SHELL is, and follow it (see the header).
# The last word read is the exit status of reattach, so a reattach that
# fails or is cut while it prints is never taken for a move. Returns when
# the window stays on HOST.
follow() {
    local word answer=() follow_status next
    while IFS= read -r -d '' word; do
        answer+=("$word")
    done < <("$i3mux" reattach ${options[@]+"${options[@]}"} --shell "$shell" --from "$host"
        printf '%s\0' "$?")
    follow_status=${answer[${#answer[@]}-1]}
    next=("${answer[@]:0:${#answer[@]}-1}")
    if [ "$follow_status" = 0 ] && [ "${#next[@]}" -gt 0 ]; then
        exec "${next[@]}"
    fi
}

# Whether stdin is a terminal with rows and columns, within a second (a new
# window may draw late). Not a terminal and no character device (a pipe):
# there is no size to wait for.
has_size() {
    local rows cols tries=10
    if ! [ -t 0 ]; then
        ! [ -c /dev/stdin ]
        return
    fi
    while [ "$tries" -gt 0 ]; do
        read -r rows cols < <(stty size 2>/dev/null) || return 1
        [ "${rows:-0}" -gt 0 ] && [ "${cols:-0}" -gt 0 ] && return 0
        sleep 0.1
        tries=$((tries - 1))
    done
    return 1
}

# Exit, after one line, unless the terminal has a size (see the header).
need_size() {
    has_size && return 0
    say "This terminal has no size (0 rows or 0 columns), or is gone: $title is not attached."
    exit 1
}

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
    need_size
    if [ -z "$remote" ]; then
        "${argv[@]}"
        status=$?
        follow
        break
    fi
    read_unit
    seen_inv=$inv seen_restarts=$restarts
    # ssh's own messages go to stderr; keep them for the error page. The
    # remote shell's output reaches the terminal through the pty on stdout.
    { ssh_error=$("${argv[@]}" 2>&1 >&3 3>&-); } 3>&1
    status=$?
    follow
    if [ "$status" -eq 255 ]; then
        recover || exit "$status"
        # The session may have moved while the link was down.
        follow
        argv=("${reattach[@]}")
        clear_screen
        continue
    fi
    [ -n "$ssh_error" ] && printf '%s\n' "$ssh_error" >&2
    if [ "$status" -eq 75 ] && [ -z "$replaced" ] && [ -z "$attach_only" ]; then
        replaced=1
        say "Starting a new shell in this window."
        continue
    fi
    break
done
elapsed=$(($(date +%s) - start))
[ -n "$no_hold" ] && exit "$status"
if [ "$status" -ne 0 ] || [ "$elapsed" -lt 2 ]; then
    printf '\n[i3mux] %s ended after %s s with exit status %s.\n' "$title" "$elapsed" "$status"
    printf '[i3mux] If the shell is already attached from another terminal or host,\n'
    printf "[i3mux] run 'i3mux terminal --force' or 'i3mux attach --force' to take it over.\n"
    printf '[i3mux] Press Enter to close this window.\n'
    read -r _ || true
fi
exit "$status"
