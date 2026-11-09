#!/bin/sh
# Assemble Overseer.app around a built binary and sign it ad hoc.
#
# Usage: scripts/bundle.sh [binary] [app dir]
# Defaults: target/release/overseer and ~/Applications/Overseer.app
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-"$root/target/release/overseer"}
app=${2:-"$HOME/Applications/Overseer.app"}

if [ ! -x "$bin" ]; then
    echo "bundle.sh: no binary at $bin (run cargo build --release -p overseer)" >&2
    exit 1
fi

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)

mkdir -p "$app/Contents/MacOS"
# Unlink before copying so a running daemon or GUI keeps its old file
# instead of having it rewritten underneath it.
rm -f "$app/Contents/MacOS/overseer"
cp "$bin" "$app/Contents/MacOS/overseer"
chmod 755 "$app/Contents/MacOS/overseer"
sed "s/__VERSION__/$version/g" "$root/packaging/Info.plist" > "$app/Contents/Info.plist"

codesign --force --sign - "$app"
echo "bundled $app ($version)"
