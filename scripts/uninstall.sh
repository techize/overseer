#!/bin/sh
# End every running session, remove the LaunchAgent, the
# ~/.local/bin/overseer link and ~/Applications/Overseer.app. State and logs
# are left in place; the sessions come back as resumable after a reinstall.
# With OVERSEER_HOME set, that home's agent, app and link instead (see
# scripts/locations.sh).
#
# Environment:
#   OVERSEER_HOME       an isolated home: its own agent is removed, never
#                       the default one
#   OVERSEER_LAUNCHCTL  the launchctl to run (default launchctl)
#   OVERSEER_APP_DIR, OVERSEER_LINK_DIR  as for install.sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
. "$root/scripts/locations.sh"
label=uk.co.techize.overseer

# Sessions kept by a holder outlive the daemon, so end them first. With a
# daemon running, stop-all ends them through its shutdown path (which keeps
# them resumable) and stops it. Whatever it did not end, or all of them when no
# daemon answers, is ended from the holders' pid files. An ended holder's exit
# file is removed so its session comes back as resumable, not as exited.
if [ -x "$app_bin" ]; then
    ov=$app_bin
else
    ov=$(command -v overseer || true)
fi
state=${OVERSEER_HOME:-"$HOME/Library/Application Support/Overseer"}
if [ -n "$ov" ]; then
    if ! stop_err=$("$ov" ctl stop-all 2>&1); then
        echo "uninstall.sh: ctl stop-all did not finish (${stop_err:-no message}); ending sessions by pid file" >&2
    fi
    alive=""
    for pidfile in "$state"/holders/*.pid; do
        [ -e "$pidfile" ] || continue
        slot=$(basename "$pidfile" .pid)
        if ! "$ov" hold --end "$pidfile"; then
            alive="$alive $slot"
            continue
        fi
        pid=$(sed -n 's/.*"holder"[^0-9]*\([0-9][0-9]*\).*/\1/p' "$pidfile" 2>/dev/null || true)
        # A holder just sent its final SIGTERM may take a moment to go.
        tries=0
        while [ -e "$pidfile" ] && [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && [ "$tries" -lt 10 ]; do
            sleep 0.2
            tries=$((tries + 1))
        done
        if [ -e "$pidfile" ] && [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            alive="$alive $slot"
        else
            rm -f "${pidfile%.pid}.exit"
        fi
    done
    if [ -n "$alive" ]; then
        echo "uninstall.sh: still running:$alive. End them before installing again; $state/holders is left in place" >&2
    else
        rm -rf "$state/holders"
    fi
elif [ -d "$state/holders" ]; then
    echo "No overseer binary; sessions kept in $state/holders are not ended."
fi

if [ -x "$app_bin" ]; then
    # It knows this home's label and plist, and honours OVERSEER_LAUNCHCTL.
    if ! "$app_bin" uninstall-agent; then
        echo "uninstall.sh: overseer uninstall-agent failed; the app and link are left in place" >&2
        exit 1
    fi
elif [ -z "${OVERSEER_HOME:-}" ]; then
    # Like uninstall-agent, only OVERSEER_HOME picks the label: with it unset
    # this is the default agent, whatever HOME is.
    "${OVERSEER_LAUNCHCTL:-launchctl}" bootout "gui/$(id -u)/$label" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/$label.plist"
else
    # An isolated home's label comes from the app binary; without it, guess
    # nothing and never touch the default agent.
    echo "No app binary; remove $OVERSEER_HOME/agent by hand."
fi

if [ -L "$link" ]; then
    rm -f "$link"
    echo "removed $link"
fi
if [ -d "$app" ]; then
    rm -r "$app"
    echo "removed $app"
fi
if [ -n "${OVERSEER_HOME:-}" ]; then
    echo "Overseer is uninstalled. State and logs in $OVERSEER_HOME are left in place."
else
    echo "Overseer is uninstalled. State in ~/Library/Application Support/Overseer and logs in ~/Library/Logs/Overseer are left in place."
fi
