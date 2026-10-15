#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
CARGO_BUILD_BUILD_DIR="$(pwd)/target/build-local" CARGO_BUILD_JOBS=4 nice -n 19 ionice -c3 flock /tmp/cargo.lock cargo build --release -p gray
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
    rm -f "$shadow"
    echo "removed stale $shadow (cargo-install shadow; $dst is the only gray)"
fi
