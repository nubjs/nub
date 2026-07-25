#!/bin/sh
# qos-global — install scripts/rustc-qos.sh as this machine's cargo rustc-wrapper:
# a stable copy at ~/.cargo/rustc-qos.sh referenced from ~/.cargo/config.toml, so
# deleting any worktree/checkout can never break machine-wide builds. Idempotent;
# darwin-only; refuses to clobber a foreign rustc-wrapper (e.g. sccache).
# `make install-dev` depends on this, so the installed copy self-heals.
set -eu
[ "$(uname)" = "Darwin" ] || { echo "qos-global: darwin-only, skipping"; exit 0; }
dir=$(cd "$(dirname "$0")" && pwd)
cfg="$HOME/.cargo/config.toml"
wrapper="$HOME/.cargo/rustc-qos.sh"
mkdir -p "$HOME/.cargo"
cp "$dir/rustc-qos.sh" "$wrapper"
chmod +x "$wrapper"
line="rustc-wrapper = \"$wrapper\""
if [ -f "$cfg" ] && grep -q 'rustc-wrapper' "$cfg"; then
  grep -qF "$line" "$cfg" && { echo "qos-global: installed (wrapper copy refreshed)"; exit 0; }
  echo "qos-global: $cfg sets a different rustc-wrapper; not touching it" >&2
  exit 1
fi
if [ -f "$cfg" ] && grep -q '^\[build\]' "$cfg"; then
  awk -v line="$line" '{ print; if (!done && $0 == "[build]") { print line; done = 1 } }' \
    "$cfg" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg"
else
  printf '\n[build]\n%s\n' "$line" >> "$cfg"
fi
echo "qos-global: installed rustc-wrapper -> $wrapper"
