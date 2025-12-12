#!/bin/bash
# color-fill.sh - Fill terminal with solid color and sleep until killed
#
# Usage: color-fill.sh <COLOR_CODE> [PATTERN]
# COLOR_CODE: ANSI color code (e.g., "41" for red bg, "42" for green bg)
# PATTERN: optional - "gradient", "checker", "solid" (default: solid)

COLOR=${1:-41}  # Default: red background
PATTERN=${2:-solid}

# Clear screen and hide cursor
clear
tput civis

# Trap to ensure cleanup on exit
cleanup() {
    tput cnorm  # Show cursor
    clear
    exit 0
}

trap cleanup INT TERM EXIT HUP

# Function to fill terminal with color
fill_terminal() {
    local ROWS=$(tput lines)
    local COLS=$(tput cols)

    # Clear and position cursor at top
    clear

    case "$PATTERN" in
        solid)
            # Fill entire terminal with solid color
            echo -ne "\033[${COLOR}m"
            for ((i=0; i<ROWS; i++)); do
                printf "%${COLS}s\n" " "
            done
            ;;
        gradient)
            # Vertical gradient using 256-color mode
            for ((i=0; i<ROWS; i++)); do
                COLOR_VAL=$((16 + i * 215 / ROWS))
                echo -ne "\033[48;5;${COLOR_VAL}m"
                printf "%${COLS}s\n" " "
            done
            echo -ne "\033[0m"
            ;;
        checker)
            # Checkerboard pattern
            for ((i=0; i<ROWS; i++)); do
                for ((j=0; j<COLS; j++)); do
                    if (( (i/4 + j/4) % 2 == 0 )); then
                        echo -ne "\033[${COLOR}m "
                    else
                        echo -ne "\033[0m "
                    fi
                done
                echo ""
            done
            ;;
        *)
            echo "Unknown pattern: $PATTERN"
            echo "Valid patterns: solid, gradient, checker"
            exit 1
            ;;
    esac
}

# Refill on window resize
trap fill_terminal WINCH

# Initial fill
fill_terminal

# Keep running and refill periodically to catch any missed resizes
while true; do
    sleep 0.5
    # Check if size changed
    NEW_ROWS=$(tput lines)
    NEW_COLS=$(tput cols)
    if [[ "$NEW_ROWS" != "$ROWS" ]] || [[ "$NEW_COLS" != "$COLS" ]]; then
        ROWS=$NEW_ROWS
        COLS=$NEW_COLS
        fill_terminal
    fi
done
