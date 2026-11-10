#!/bin/sh
# Fake Claude Code session for daemon tests.
printf 'READY\n'
printf '\033[38;5;208m256color\033[0m \033[38;2;255;100;0mtruecolor\033[0m\n'
while IFS= read -r line; do
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
        *)
            printf 'ECHO:%s\n' "$line"
            ;;
    esac
done
