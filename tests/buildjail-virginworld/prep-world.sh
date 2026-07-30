#!/usr/bin/env bash
# ONE-TIME world preparation. The maintainer relaxed the bar to allow a slow one-time spin-up, so
# this is the slow half: fetch a base rootfs, add the compiler toolchain node-gyp needs, drop in the
# Node nub would provision. The result is a reusable lowerdir; per-install runs overlay a tmpfs on
# top of it and are ~free.
#
# glibc choice is load-bearing (research doc §2a): the world's glibc must be <= the oldest host it
# must produce loadable .node files for. bullseye = 2.31, below both ubuntu-22.04 (2.35) and
# ubuntu-24.04 (2.39).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORLD="${1:?world directory}"
NODE_VER="${NODE_VER:-22.15.0}"
BASE_IMAGE="${BASE_IMAGE:-library/debian}"
BASE_TAG="${BASE_TAG:-bullseye-slim}"
ARCH="$(uname -m)"; case "$ARCH" in x86_64) OCI_ARCH=amd64; NODE_ARCH=x64;; aarch64) OCI_ARCH=arm64; NODE_ARCH=arm64;; *) echo "unsupported arch $ARCH" >&2; exit 1;; esac

step() { echo; echo "=== $* ==="; }
t() { local l="$1"; shift; local s=$(date +%s.%N); "$@"; local e=$(date +%s.%N); printf 'TIME %-28s %.2fs\n' "$l" "$(echo "$e - $s" | bc)"; }

step "1. base rootfs: $BASE_IMAGE:$BASE_TAG ($OCI_ARCH)"
rm -rf "$WORLD"; mkdir -p "$WORLD"
t rootfs-pull "$HERE/pull-rootfs.sh" "$BASE_IMAGE" "$BASE_TAG" "$WORLD" "$OCI_ARCH"
echo "rootfs size: $(du -sh "$WORLD" | cut -f1)"
echo "world glibc: $(strings "$WORLD"/lib/*/libc.so.6 2>/dev/null | grep -m1 '^GNU C Library' || echo unknown)"

step "2. provisioned Node v$NODE_VER at /opt/node"
mkdir -p "$WORLD/opt/node"
t node-download bash -c "curl -fsSL https://nodejs.org/dist/v$NODE_VER/node-v$NODE_VER-linux-$NODE_ARCH.tar.xz | tar -xJ --no-same-owner --strip-components=1 -C '$WORLD/opt/node'"
"$WORLD/opt/node/bin/node" --version >/dev/null 2>&1 && echo "node binary runs on the host too: $("$WORLD/opt/node/bin/node" --version)"

step "3. compiler toolchain (node-gyp needs cc/c++/make/python3)"
mkdir -p "$WORLD/work" "$WORLD/home/build" "$WORLD/opt"
APT='export DEBIAN_FRONTEND=noninteractive; apt-get -o APT::Sandbox::User=root -o Acquire::Retries=3 update -qq && apt-get -o APT::Sandbox::User=root -o Dpkg::Use-Pty=0 install -y --no-install-recommends build-essential python3 ca-certificates >/dev/null'
TOOLCHAIN_PRIV=unknown
if t toolchain-unprivileged "$HERE/jail" --root "$WORLD" --rw-lower --cwd / -- /bin/sh -c "$APT"; then
  TOOLCHAIN_PRIV=none
else
  echo "unprivileged apt in the world FAILED; retrying with sudo chroot (one-time root)"
  sudo cp /etc/resolv.conf "$WORLD/etc/resolv.conf"
  sudo mount --bind /dev "$WORLD/dev"; sudo mount -t proc proc "$WORLD/proc"
  t toolchain-sudo sudo chroot "$WORLD" /bin/sh -c "$APT"
  sudo umount "$WORLD/proc" "$WORLD/dev"
  sudo chown -R "$(id -u):$(id -g)" "$WORLD"
  TOOLCHAIN_PRIV=root
fi
echo "TOOLCHAIN_PRIVILEGE=$TOOLCHAIN_PRIV"

step "4. world contents"
echo "total size: $(du -sh "$WORLD" | cut -f1)"
ls -1 "$WORLD"
echo "TOOLCHAIN_PRIVILEGE=$TOOLCHAIN_PRIV" > "$WORLD/.prep-info"
