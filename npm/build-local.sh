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
echo "Building for ${PLATFORM}-${ARCH} → $PKG_DIR"

# 1. Build release binary
cargo build --release -p nub-cli

# 2. Copy binary
mkdir -p "$PKG_DIR/bin"
cp "$REPO_ROOT/target/release/nub" "$PKG_DIR/bin/nub"
chmod +x "$PKG_DIR/bin/nub"

# 3. Copy runtime
rm -rf "$PKG_DIR/runtime"
cp -r "$REPO_ROOT/runtime" "$PKG_DIR/runtime"

# 4. Vendor node_modules — pure-JS deps only. The TS/JSX transpiler + module
# detection, tsconfig discovery/parse + the additive TS-resolver, AND the transpile
# cache are now IN-PROCESS in nub-native (Rust addon, oxc compiled in), so
# oxc-transform / oxc-parser and get-tsconfig (+ its resolve-pkg-maps dep) are NO
# LONGER vendored. nub loads zero npm packages internally now.
rm -rf "$PKG_DIR/runtime/node_modules"
mkdir -p "$PKG_DIR/runtime/node_modules"
# @oxc-project/runtime — oxc-transform emits helper imports from it (e.g.
# `@oxc-project/runtime/helpers/decorate`) for decorators; oxc has zero deps, so
# it never arrives transitively (A30). cp the package symlink (cp -r follows the
# source symlink) into a freshly-made scope dir.
mkdir -p "$PKG_DIR/runtime/node_modules/@oxc-project"
cp -r "$REPO_ROOT/node_modules/@oxc-project/runtime" "$PKG_DIR/runtime/node_modules/@oxc-project/"
# Clobbered/lazy polyfills nub provides as internal deps: URLPattern (A39, Node
# 22.x), Temporal (A37), and Float16Array (D5/A25, Node 22.x). @js-temporal/
# polyfill pulls jsbi (its only dep), kept flat alongside so the package resolves
# it; @petamoriken/float16 has zero deps.
cp -r "$REPO_ROOT/node_modules/urlpattern-polyfill" "$PKG_DIR/runtime/node_modules/"
mkdir -p "$PKG_DIR/runtime/node_modules/@js-temporal"
cp -r "$REPO_ROOT/node_modules/@js-temporal/polyfill" "$PKG_DIR/runtime/node_modules/@js-temporal/"
cp -r "$REPO_ROOT/node_modules/jsbi" "$PKG_DIR/runtime/node_modules/"
mkdir -p "$PKG_DIR/runtime/node_modules/@petamoriken"
cp -r "$REPO_ROOT/node_modules/@petamoriken/float16" "$PKG_DIR/runtime/node_modules/@petamoriken/"

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
