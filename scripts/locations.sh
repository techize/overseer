# Where install.sh puts the app and the CLI link, and where uninstall.sh
# removes them from. Sourced, not run.
#
# Default home: ~/Applications/Overseer.app and ~/.local/bin/overseer.
# A custom home (OVERSEER_HOME set): $OVERSEER_HOME/Applications and
# $OVERSEER_HOME/bin, so an isolated install never replaces or removes the
# app the default agent runs. OVERSEER_APP_DIR and OVERSEER_LINK_DIR
# override either.

if [ -n "${OVERSEER_HOME:-}" ]; then
    # Absolute, so the link and the printed paths work from anywhere.
    case $OVERSEER_HOME in
        /*) ;;
        *) OVERSEER_HOME="$(pwd)/$OVERSEER_HOME" ;;
    esac
    export OVERSEER_HOME
    app_dir=${OVERSEER_APP_DIR:-"$OVERSEER_HOME/Applications"}
    link_dir=${OVERSEER_LINK_DIR:-"$OVERSEER_HOME/bin"}
else
    app_dir=${OVERSEER_APP_DIR:-"$HOME/Applications"}
    link_dir=${OVERSEER_LINK_DIR:-"$HOME/.local/bin"}
fi
app="$app_dir/Overseer.app"
app_bin="$app/Contents/MacOS/overseer"
link="$link_dir/overseer"

# A path with $HOME shown as ~.
tilde() {
    case $1 in
        "$HOME") echo "~" ;;
        "$HOME"/*) echo "~${1#"$HOME"}" ;;
        *) echo "$1" ;;
    esac
}
