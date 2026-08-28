#!/bin/sh
# qos-global — install scripts/rustc-qos.sh as this machine's cargo rustc wrapper:
# a stable copy at ~/.cargo/rustc-qos.sh referenced from ~/.cargo/config.toml, so
# deleting any worktree/checkout can never break machine-wide builds. Idempotent;
# darwin-only; refuses to clobber a foreign wrapper (e.g. sccache).
# `make install-dev` depends on this, so the installed copy self-heals.
#
# BOTH KEYS ARE INSTALLED, and the second one is not redundant. `rust-build.sh`
# historically blanked RUSTC_WRAPPER, and 58 of 61 live worktrees still carry
# that version (measured 2026-08-19) — every build launched from one opts itself
# out of `rustc-wrapper` and out of the global concurrency semaphore with it.
# Those scripts do NOT blank RUSTC_WORKSPACE_WRAPPER, so registering the same
# wrapper there keeps the workspace crates governed no matter how stale the
# checkout is. Coverage is partial by construction (workspace members only, so
# not vendor/aube or crates.io deps) — it is a floor under stale checkouts, not
# a replacement for the wrapper proper. The wrapper is re-entrant, so a workspace
# crate running through both hops still takes exactly one token.
set -eu
[ "$(uname)" = "Darwin" ] || { echo "qos-global: darwin-only, skipping"; exit 0; }
dir=$(cd "$(dirname "$0")" && pwd)
cfg="$HOME/.cargo/config.toml"
wrapper="$HOME/.cargo/rustc-qos.sh"
# SECOND NAME, DELIBERATELY. Every stale checkout carries its own qos-global.sh
# whose FIRST act is an unconditional `cp` over ~/.cargo/rustc-qos.sh, and
# `make install-dev` calls it. So one `make install-dev` from any of the 58
# worktrees still on the old script silently downgrades the machine-wide wrapper
# back to a QoS-only clamp and the concurrency semaphore just disappears -- no
# error, no output, load climbs again an hour later. Those scripts have never
# heard of this path, so installing the same wrapper under a second name and
# binding THAT to rustc-workspace-wrapper keeps a governor alive through the
# clobber. Both names are the same file, and the wrapper is re-entrant, so a
# crate that runs through both hops still takes exactly one token.
governor="$HOME/.cargo/rustc-gov.sh"
mkdir -p "$HOME/.cargo"
cp "$dir/rustc-qos.sh" "$wrapper"
cp "$dir/rustc-qos.sh" "$governor"
chmod +x "$wrapper" "$governor"

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
