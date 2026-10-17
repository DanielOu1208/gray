#!/bin/sh

set -eu

SITE="${GRAY_CDN_URL:-https://gray.alignment.id/dl}"
SITE="${SITE%/dl}"

if command -v curl >/dev/null 2>&1; then
    SRC=$(curl -fsSL "${SITE}/install.sh")
elif command -v wget >/dev/null 2>&1; then
    SRC=$(wget -qO- "${SITE}/install.sh")
else
    echo "need curl or wget to download" >&2
    exit 1
fi

printf '%s\n' "$SRC" | GRAY_INSTALL_PLUGINS=0 sh -s -- "$@"
