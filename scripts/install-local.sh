#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
CARGO_BUILD_JOBS=4 nice -n 19 ionice -c3 flock /tmp/cargo.lock cargo build --release -p gray
src="$(pwd)/target/release/gray"
dst="$HOME/.local/bin/gray"
tmp="$dst.tmp.$$"
trap 'rm -f "$tmp"' EXIT
cp "$src" "$tmp"
chmod 755 "$tmp"
mv "$tmp" "$dst"
trap - EXIT
echo "installed $src -> $dst"
shadow="$HOME/.cargo/bin/gray"
if [ -e "$shadow" ]; then
    echo ""
    echo "WARNING: $shadow exists — if it sits earlier on PATH it shadows $dst and new shells launch the old build."
    echo "  fix: rm $shadow"
fi
