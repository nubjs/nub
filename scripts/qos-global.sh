#!/bin/sh
# qos-global — install scripts/rustc-qos.sh as this machine's cargo rustc wrapper:
# a stable copy at ~/.cargo/rustc-qos.sh referenced from ~/.cargo/config.toml, so
# deleting any worktree/checkout can never break machine-wide builds. Idempotent;
# darwin-only; refuses to clobber a foreign wrapper (e.g. sccache).
# `make install-dev` depends on this, so the installed copy self-heals.
#
# BOTH KEYS ARE INSTALLED, and the second one is not redundant — it is the floor
# under STALE CHECKOUTS, which come in two shapes:
#   - `rust-build.sh` historically blanked RUSTC_WRAPPER, and 58 of 61 live
#     worktrees still carried that version on 2026-08-19. Such a build opts
#     itself out of `rustc-wrapper`, but not out of RUSTC_WORKSPACE_WRAPPER, so
#     binding the same wrapper there keeps its workspace crates governed
#     (partial by construction: not vendor/aube or crates.io deps).
#   - Every stale checkout's qos-global.sh copies its own older rustc-qos.sh
#     over ~/.cargo/rustc-qos.sh on `make install-dev`, silently downgrading the
#     machine-wide governor. Checkouts older than this second name leave
#     ~/.cargo/rustc-gov.sh alone, so a governor survives under that name;
#     newer stale checkouts clobber both, which `make build-status` reports as
#     STALE WRAPPER. The wrapper's body is one compound command precisely so a
#     copy over the live file cannot truncate a wrapper mid-compile.
# The wrapper is re-entrant, so a workspace crate running through both hops
# still takes exactly one token.
set -eu
[ "$(uname)" = "Darwin" ] || { echo "qos-global: darwin-only, skipping"; exit 0; }
dir=$(cd "$(dirname "$0")" && pwd)
cfg="$HOME/.cargo/config.toml"
wrapper="$HOME/.cargo/rustc-qos.sh"
governor="$HOME/.cargo/rustc-gov.sh"    # the second name, see above
mkdir -p "$HOME/.cargo"
# Write-then-rename, never `cp` over the live file: sh reads a script
# incrementally, so every wrapper mid-compile machine-wide would read the new
# bytes at its old offset. A rename gives them their old inode to finish on.
for dst in "$wrapper" "$governor"; do
  cp "$dir/rustc-qos.sh" "$dst.tmp.$$"
  chmod +x "$dst.tmp.$$"
  mv "$dst.tmp.$$" "$dst"
done

# Refuse to fight a foreign wrapper (sccache and friends own the same slot).
if [ -f "$cfg" ] && grep -q '^[[:space:]]*rustc-wrapper' "$cfg" \
  && ! grep -qF "rustc-wrapper = \"$wrapper\"" "$cfg"; then
  echo "qos-global: $cfg sets a different rustc-wrapper; not touching it" >&2
  exit 1
fi

# Ensure one `<key> = "$wrapper"` line exists under [build], creating the section
# if needed. Split out because the two keys are added independently — the second
# was introduced later, so an existing config already carries only the first.
ensure_key() {
  _k=$1
  _line="$_k = \"$2\""
  grep -qF "$_line" "$cfg" 2>/dev/null && return 0
  if [ -f "$cfg" ] && grep -q '^\[build\]' "$cfg"; then
    awk -v line="$_line" \
      '{ print; if (!done && $0 == "[build]") { print line; done = 1 } }' \
      "$cfg" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg"
  else
    printf '\n[build]\n%s\n' "$_line" >> "$cfg"
  fi
}

ensure_key rustc-wrapper "$wrapper"
ensure_key rustc-workspace-wrapper "$governor"
echo "qos-global: rustc-wrapper -> $wrapper"
echo "qos-global: rustc-workspace-wrapper -> $governor (survives a stale install-dev)"
