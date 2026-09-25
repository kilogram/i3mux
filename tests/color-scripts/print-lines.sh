#!/bin/sh
# print-lines.sh - Print 300 numbered lines, then idle until killed.
#
# The scrollback test runs this with `i3mux terminal -e`, detaches, attaches,
# and checks that the lines are on screen again. Each line is wide so that a
# missing or shifted replay changes many pixels.
i=1
while [ "$i" -le 300 ]; do
    printf 'SCROLLBACK-LINE-%03d ================================================================\n' "$i"
    i=$((i + 1))
done
exec sleep 100000
