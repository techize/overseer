#!/bin/sh
# Remove the LaunchAgent, the ~/.local/bin/overseer link and
# ~/Applications/Overseer.app. State and logs are left in place. With
# OVERSEER_HOME set, that home's agent, app and link instead (see
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
