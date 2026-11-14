#!/bin/sh
# Build Overseer, assemble ~/Applications/Overseer.app, link
# ~/.local/bin/overseer and install the LaunchAgent. No admin rights needed.
#
# Environment:
#   YES=1                  restart the daemon without asking, even with
#                          sessions running (install-agent --yes)
#   NO_RESTART=1           keep a running daemon (install-agent --no-restart)
#   OVERSEER_INSTALL_RUST=1  install Rust through rustup without asking
#   OVERSEER_INSTALL_BINARY=<path>  use this binary instead of building
#   OVERSEER_CURL          the command that fetches rustup (default curl)
#   OVERSEER_HOME          an isolated home: its agent and logs live there
#   CARGO_HOME, CARGO_TARGET_DIR  as cargo uses them
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
app="$HOME/Applications/Overseer.app"
app_bin="$app/Contents/MacOS/overseer"
link="$HOME/.local/bin/overseer"
cargo_home=${CARGO_HOME:-"$HOME/.cargo"}
rustup_args="-y --no-modify-path --profile minimal --default-toolchain none"
rustup_cmd="curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- $rustup_args"

work=$(mktemp -d "${TMPDIR:-/tmp}/overseer-install.XXXXXX")
trap 'rm -rf "$work"' EXIT

have_cargo() {
    command -v cargo >/dev/null 2>&1 || [ -x "$cargo_home/bin/cargo" ]
}

# Decision 6: install Rust for this user when cargo is nowhere to be found.
# Downloading and running a remote script is never silent: on a terminal it
# asks, and without one it needs OVERSEER_INSTALL_RUST=1.
installed_rust=0
install_rust() {
    if [ "${OVERSEER_INSTALL_RUST:-}" != 1 ]; then
        if [ -t 0 ]; then
            printf '%s' "Rust is not installed. Install it for your user with rustup now? This downloads and runs https://sh.rustup.rs (no admin rights needed). [y/N] "
            read -r answer || answer=
            case $answer in
                y | Y | yes | Yes | YES) ;;
                *)
                    echo "Rust was not installed. To install it yourself, run:"
                    echo "  $rustup_cmd"
                    echo "then run make install again."
                    exit 1
                    ;;
            esac
        else
            echo "Rust is not installed. To install it for your user (no admin rights needed), run:"
            echo "  $rustup_cmd"
            echo "then run make install again, or run OVERSEER_INSTALL_RUST=1 make install to let it do this."
            exit 1
        fi
    fi
    echo "Rust is not installed. Installing it for your user with rustup (no admin rights needed)."
    CARGO_HOME=$cargo_home
    export CARGO_HOME
    "${OVERSEER_CURL:-curl}" --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs > "$work/rustup.sh"
    # shellcheck disable=SC2086 # rustup_args is a fixed word list
    sh "$work/rustup.sh" $rustup_args
    if [ ! -x "$cargo_home/bin/cargo" ]; then
        echo "install.sh: rustup finished but there is no cargo at $cargo_home/bin/cargo" >&2
        exit 1
    fi
    installed_rust=1
}

if [ -n "${OVERSEER_INSTALL_BINARY:-}" ]; then
    bin=$OVERSEER_INSTALL_BINARY
else
    have_cargo || install_rust
    bin="${CARGO_TARGET_DIR:-$root/target}/release/overseer"
    # $CARGO_HOME/bin goes first so rustup's cargo and the toolchain that
    # rust-toolchain.toml picks are the ones used. Only the build sees it,
    # not the PATH install-agent records for the daemon.
    (
        PATH="$cargo_home/bin:$PATH"
        export PATH
        cd "$root"
        cargo build --release -p overseer
    )
fi

"$root/scripts/bundle.sh" "$bin" "$app"
mkdir -p "$(dirname "$link")"
ln -sfn "$app_bin" "$link"

set -- install-agent
[ "${YES:-}" = 1 ] && set -- "$@" --yes
[ "${NO_RESTART:-}" = 1 ] && set -- "$@" --no-restart

# Show install-agent's output as it comes (its restart prompt included; the
# prompt reads stdin, which stays yours) and keep a copy to read the label.
{
    rc=0
    "$app_bin" "$@" || rc=$?
    echo "$rc" > "$work/status"
} | tee "$work/out"
rc=$(cat "$work/status")
if [ "$rc" != 0 ]; then
    echo "install.sh: overseer install-agent failed (exit $rc)" >&2
    exit "$rc"
fi

# install-agent prints "installed <plist> as <label> (runs <binary>)".
label=$(sed -n 's/^installed .* as \([^ ]*\) (runs .*$/\1/p' "$work/out" | tail -n 1)
[ -n "$label" ] || label="(see install-agent's output above)"
if [ -n "${OVERSEER_HOME:-}" ]; then
    logs="$OVERSEER_HOME/logs"
else
    logs="~/Library/Logs/Overseer"
fi

echo
echo "Overseer is installed."
echo "  App:    $app"
echo "  CLI:    $link (add ~/.local/bin to PATH if it is not there)"
echo "  Daemon: LaunchAgent $label, logs in $logs"
echo "Open Overseer from ~/Applications in Finder, or run: open ~/Applications/Overseer.app"
if [ "$installed_rust" = 1 ]; then
    echo "Add ~/.cargo/bin to your PATH to run cargo yourself."
fi
