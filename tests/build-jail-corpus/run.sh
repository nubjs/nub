#!/usr/bin/env bash
# Run the lifecycle contract and real-package compatibility pairs against one artifact.
set -euo pipefail
: "${NUB_BIN:?set NUB_BIN to the matching built Nub artifact}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
node --test "$here/contract.mjs" "$here/private-registry.mjs" "$here/descendants.mjs" "$here/cache-modes.mjs" "$here/layouts.mjs"
node "$here/packages.mjs"
node --test "$here/fetched-native.mjs"
