#!/bin/sh
# i3mux terminal wrapper. Usage: wrapper.sh TITLE ARGV...
#
# Sets the window title once and runs ARGV as given (no shell parses it).
# Keeps the window open with a hint when ARGV fails or ends within 2 s.
# shpool refuses a second client of a shell with exit status 0, so a quick
# end is the only sign that another terminal or host took the shell first.
title=$1
shift
printf '\033]0;%s\007' "$title"
start=$(date +%s)
"$@"
status=$?
elapsed=$(( $(date +%s) - start ))
if [ "$status" -ne 0 ] || [ "$elapsed" -lt 2 ]; then
    printf '\n[i3mux] %s ended after %s s with exit status %s.\n' "$title" "$elapsed" "$status"
    printf '[i3mux] If the shell is already attached from another terminal or host,\n'
    printf "[i3mux] run 'i3mux terminal --force' or 'i3mux attach --force' to take it over.\n"
    printf '[i3mux] Press Enter to close this window.\n'
    read -r _ || true
fi
exit "$status"
