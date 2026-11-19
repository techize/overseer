# Sourced by install.sh and bundle.sh.
#
# target_dir <root> prints the folder cargo builds into for the workspace at
# <root>. Cargo reads it from CARGO_TARGET_DIR or from a .cargo/config.toml
# in <root> or any folder above it, so ask cargo rather than guess. Without
# a working cargo it falls back to CARGO_TARGET_DIR, then <root>/target.
target_dir() {
    dir=$(cd "$1" && cargo metadata --no-deps --format-version 1 2>/dev/null |
        sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -n 1) || dir=
    [ -n "$dir" ] || dir=${CARGO_TARGET_DIR:-"$1/target"}
    printf '%s\n' "$dir"
}
