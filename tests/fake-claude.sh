#!/bin/sh
# Fake Claude Code session for daemon tests.
#
# `fake-claude.sh box <dir>` also draws Claude Code's input box at start and
# on every resize, logging each draw to <dir>/draws, and after a resize
# draws it again every 100 ms for 1s.
#
# Lines it understands besides plain text (echoed as ECHO:<line>):
#   quit                 exit 3
#   flood                print 6 MiB, then exit 0
#   later <path>         1s after <path> appears, print a marker, then
#                        create <path>.done
#   exitwhen <path> <n>  once <path> appears, exit <n>
#   burstwhen <path>     once <path> appears, print 8 MiB, then create
#                        <path>.done
draw() {
    cols=$(stty size < /dev/tty | cut -d' ' -f2)
    rule=$(awk -v n="$cols" 'BEGIN { for (i = 0; i < n; i++) printf "\342\224\200" }')
    printf '\033[2J\033[H\033[?2004hDRAWN\r\n\r\n%s\r\n\342\235\257 \r\n%s\r\n  ? for shortcuts' "$rule" "$rule"
    echo drawn >> "$draws"
}

redraw() {
    draw
    (
        i=0
        while [ $i -lt 10 ]; do
            sleep 0.1
            draw
            i=$((i + 1))
        done
    ) &
}

wait_for() {
    while [ ! -e "$1" ]; do sleep 0.05; done
}

printf 'READY\n'
printf '\033[38;5;208m256color\033[0m \033[38;2;255;100;0mtruecolor\033[0m\n'
if [ "$1" = box ]; then
    draws="$2/draws"
    trap redraw WINCH
    draw
fi
while :; do
    IFS= read -r line
    status=$?
    # A signal handled during the read cuts it short; read again.
    if [ $status -gt 128 ]; then
        continue
    fi
    [ $status -eq 0 ] || exit 0
    line=${line%"$(printf '\r')"}
    case "$line" in
        quit)
            exit 3
            ;;
        flood)
            awk 'BEGIN {
                s = sprintf("%1023s", "");
                gsub(/ /, "x", s);
                for (i = 0; i < 6 * 1024; i++) print s;
                print "FLOOD-END";
            }'
            exit 0
            ;;
        "later "*)
            path=${line#later }
            (
                wait_for "$path"
                sleep 1
                printf 'DOWN%s\n' -MARK
                : > "$path.done"
            ) &
            ;;
        "exitwhen "*)
            set -- ${line#exitwhen }
            wait_for "$1"
            exit "$2"
            ;;
        "burstwhen "*)
            path=${line#burstwhen }
            wait_for "$path"
            awk 'BEGIN {
                s = sprintf("%1023s", "");
                gsub(/ /, "x", s);
                for (i = 0; i < 8 * 1024; i++) print s;
            }'
            : > "$path.done"
            ;;
        *)
            printf 'ECHO:%s\n' "$line"
            ;;
    esac
done
