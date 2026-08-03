#!/bin/bash
# Build and package nub for the current platform.
# Usage: ./npm/build-local.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PLATFORM="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$PLATFORM" in darwin) ;; linux) ;; *) echo "Unsupported: $PLATFORM"; exit 1 ;; esac
case "$ARCH" in arm64|aarch64) ARCH="arm64" ;; x86_64|amd64) ARCH="x64" ;; *) echo "Unsupported: $ARCH"; exit 1 ;; esac

PKG_DIR="$REPO_ROOT/npm/nub-${PLATFORM}-${ARCH}"
echo "Building for ${PLATFORM}-${ARCH} → $PKG_DIR (single binary, embedded runtime)"

# Single-binary build: the runtime is EMBEDDED in the binary, so we stage it into
# the repo's runtime/ FIRST, then build nub-cli with --features embed-runtime so
# nub-core's build.rs tars + zstd-embeds it. `make npm-build` runs `make build`
# (= addon) first, so runtime/addons/nub-native.node is already staged here.

# 1. Vendor node_modules into the REPO runtime/ (embedded, not a $PKG sidecar).
#    Pure-JS deps only — oxc is compiled into the addon. @oxc-project/runtime
#    supplies the emit-helper imports; the rest are web-API polyfills.
rm -rf "$REPO_ROOT/runtime/node_modules"
mkdir -p "$REPO_ROOT/runtime/node_modules/@oxc-project" \
         "$REPO_ROOT/runtime/node_modules/@js-temporal" \
         "$REPO_ROOT/runtime/node_modules/@petamoriken"
cp -RL "$REPO_ROOT/node_modules/@oxc-project/runtime" "$REPO_ROOT/runtime/node_modules/@oxc-project/"
cp -RL "$REPO_ROOT/node_modules/urlpattern-polyfill" "$REPO_ROOT/runtime/node_modules/"
cp -RL "$REPO_ROOT/node_modules/@js-temporal/polyfill" "$REPO_ROOT/runtime/node_modules/@js-temporal/"
cp -RL "$REPO_ROOT/node_modules/jsbi" "$REPO_ROOT/runtime/node_modules/"
cp -RL "$REPO_ROOT/node_modules/@petamoriken/float16" "$REPO_ROOT/runtime/node_modules/@petamoriken/"

# 2. Build the release binary with the runtime embedded.
cargo build --release -p nub-cli --features embed-runtime

# 3. Copy ONLY the binary, under ONE name. No runtime/ sidecar, and no second `nubx`
#    copy: the launcher passes the verb in `__NUB_ARGV0` (npm/nub/bin/launch.js), so
#    nubx no longer needs a separately-named file. Halves the package.
rm -rf "$PKG_DIR/runtime"
mkdir -p "$PKG_DIR/bin"
rm -f "$PKG_DIR/bin/nubx"
cp "$REPO_ROOT/target/release/nub" "$PKG_DIR/bin/nub"
chmod +x "$PKG_DIR/bin/nub"

echo ""
echo "✓ Platform package ready: $PKG_DIR ($(du -sh "$PKG_DIR" | cut -f1))"
echo ""
echo "To publish locally:"
echo "  cd $PKG_DIR && npm pack"
echo "  cd $REPO_ROOT/npm/nub && npm pack"
echo "  npm install -g ./$PKG_DIR/*.tgz ./npm/nub/*.tgz"
echo ""
echo "To publish to npm:"
echo "  cd $PKG_DIR && npm publish --access public"
echo "  cd $REPO_ROOT/npm/nub && npm publish --access public"
