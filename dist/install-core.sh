#!/bin/sh

set -eu

curl -fsSL "https://gray.alignment.id/install.sh" | GRAY_INSTALL_PLUGINS=0 sh -s -- "$@"
