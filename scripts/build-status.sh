#!/bin/sh
# build-status — one-screen answer to "why is this machine saturated?" and
# "why is my build not starting?"
#
# Exists because the 2026-08-19 saturation was invisible from inside any single
# agent session: every build was individually well-behaved (all --profile fast,
# all QoS-clamped, all under `jobs = 6`) and the damage was purely their SUM.
# Nothing printed the sum. Three rows matter most: the build slots (who is
# compiling, who is queued behind them, and for how long), a STALE WRAPPER line
# (an older checkout's `make install-dev` downgraded the installed governor),
# and `partial` cargo rows (a build launched from a stale checkout that blanks
# RUSTC_WRAPPER runs its dependency crates outside the cap).
set -u

# `ps | grep` rather than pgrep throughout (shellcheck SC2009): pgrep matches
# against a truncated comm on darwin and reported 0 rustc processes on this host
# while ps showed 40. An undercount here reads as "the machine is idle", which is
# the one wrong answer this script exists to prevent.
ncpu=$( { sysctl -n hw.ncpu || nproc; } 2>/dev/null || echo 4 )
sem=${NUB_RUSTC_SEM_DIR:-$HOME/.cache/nub/rustc-sem}
limit=${NUB_RUSTC_LIMIT:-6}
bdir=${NUB_BUILD_SEM_DIR:-$HOME/.cache/nub/build-sem}
bslots=${NUB_BUILD_SLOTS:-2}
cfg="$HOME/.cargo/config.toml"

cargos=$(ps -Ao command= | grep -c '[b]in/cargo')
rustcs=$(ps -Ao command= | grep -c '[b]in/rustc')
# The wrapper is installed under two names (see qos-global.sh); count both.
wrappers=$(ps -Ao command= | grep -c -e '[r]ustc-qos.sh' -e '[r]ustc-gov.sh')
held=$(find "$sem" -mindepth 1 -maxdepth 1 -type d -name '[0-9]*' 2>/dev/null | wc -l | tr -d ' ')
runnable=$(ps -Ao state= | cut -c1 | grep -c R)
# The env of a live cargo, one `KEY=value` per line, for the wrapper-key and
# worktree tells below; `tree` is the last path component of its PWD, which is
# what tells identical `cargo test -p nub-cli` rows apart.
envof() { ps eww -o command= -p "$1" 2>/dev/null | tr ' ' '\n'; }
tree() { envof "$1" | sed -n 's|^PWD=.*/||p' | head -1; }

printf '\n== host ==\n'
printf '  cores %s   %s\n' "$ncpu" "$(uptime | sed 's/.*load averages*://')"
printf '  runnable threads %s   rustc %s   cargo %s\n' "$runnable" "$rustcs" "$cargos"
printf '  disk %s\n' "$(df -h /System/Volumes/Data 2>/dev/null | awk 'NR==2{print $4" free ("$5" used)"}')"

printf '\n== build slots (at most %s builds compile at once; the rest queue) ==\n' "$bslots"
if ! grep -q 'rustc-wrapper = ' "$cfg" 2>/dev/null; then
  printf '  NOT ACTIVE — run: make qos-global\n'
elif [ -e "$bdir/off" ]; then
  printf '  OFF SWITCH SET (%s): every build compiles unqueued — run: make build-slots-on\n' "$bdir/off"
else
  now=$(date +%s)
  for d in "$bdir"/slot/*/; do
    [ -d "$d" ] || continue
    p=$(cat "$d/pid" 2>/dev/null)
    st=$(cat "$d/stamp" 2>/dev/null)
    if [ -z "$st" ]; then
      printf '  slot %s  NO STAMP (a claim in progress, or the corpse of one; retired once a minute old)\n' "$(basename "$d")"
      continue
    fi
    if [ -n "$p" ] && ! kill -0 "$p" 2>/dev/null; then
      # Reclaim is lazy — the next wrapper to poll retires this — so a dead
      # holder on a quiet host is expected, not a leak.
      printf '  slot %s  cargo %-7s DEAD (retired by the next build to poll)\n' "$(basename "$d")" "$p"
      continue
    fi
    live=0
    for m in "$d"active/*; do [ -e "$m" ] && kill -0 "${m##*/}" 2>/dev/null && live=$((live + 1)); done
    printf '  slot %s  cargo %-7s %8s  %s compiling, last start %ss ago  %s  [%s]\n' \
      "$(basename "$d")" "$p" "$(ps -o etime= -p "$p" 2>/dev/null | tr -d ' ')" "$live" "$((now - st))" \
      "$(ps -o command= -p "$p" 2>/dev/null | sed 's|^[^ ]*/bin/||' | cut -c1-60)" "$(tree "$p")"
  done
  n=0
  for t in "$bdir"/queue/*; do
    [ -e "$t" ] || continue
    q=${t##*/}
    case $q in *.*) continue ;; esac
    # Read once: the owner truncates-then-writes and finally removes this file,
    # so it can be empty or gone between the guard above and here.
    first=""; { read -r first < "$t"; } 2>/dev/null || first=""
    [ -n "$first" ] || continue
    n=$((n + 1))
    printf '  queued  cargo %-7s waiting %4ss  %s  [%s]\n' "$q" "$((now - first))" \
      "$(ps -o command= -p "$q" 2>/dev/null | sed 's|^[^ ]*/bin/||' | cut -c1-60)" "$(tree "$q")"
  done
  printf '  %s/%s slots in use, %s queued\n' \
    "$(find "$bdir/slot" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' ')" "$bslots" "$n"
fi

# The layer holds only for builds that inherited THIS wrapper. Every stale
# checkout's qos-global.sh copies its own older rustc-qos.sh over the installed
# one on `make install-dev`, so compare versions here rather than assume.
want=$(sed -n 's/^# rustc-qos-version: \([0-9]*\).*/\1/p' "$(dirname "$0")/rustc-qos.sh" 2>/dev/null | head -1)
have=$(sed -n 's/^# rustc-qos-version: \([0-9]*\).*/\1/p' "$HOME/.cargo/rustc-qos.sh" 2>/dev/null | head -1)
if [ -n "$want" ] && [ "${have:-0}" -lt "$want" ] 2>/dev/null; then
  printf '  STALE WRAPPER: ~/.cargo/rustc-qos.sh is v%s, this tree ships v%s — builds since the downgrade run an older protocol; run: make qos-global\n' "${have:-0}" "$want"
fi

printf '\n== global rustc semaphore ==\n'
if [ -d "$sem" ]; then
  printf '  %s/%s tokens held   %s wrapper shells (held + waiting)\n' "$held" "$limit" "$wrappers"
else
  printf '  NOT ACTIVE — run: make qos-global\n'
fi

printf '\n== live cargo invocations ==\n'
partial=0
for pid in $(ps -Ao pid=,command= | awk '/[b]in\/cargo/ {print $1}'); do
  etime=$(ps -o etime= -p "$pid" 2>/dev/null | tr -d ' ')
  rest=$(ps -o command= -p "$pid" 2>/dev/null)
  [ -n "$rest" ] || continue
  env=$(envof "$pid")
  w=$(printf '%s\n' "$env" | grep '^RUSTC_WRAPPER=' | head -1)
  ww=$(printf '%s\n' "$env" | grep -x -e 'NUB_BUILD_FG=1' -e 'RUSTC_WORKSPACE_WRAPPER=' | head -1)
  # An explicitly EMPTY RUSTC_WRAPPER is cargo's documented "no wrapper" (an
  # ABSENT variable means "inherit"), so such a build is outside the full
  # semaphore -- but NOT ungoverned: qos-global also registers the wrapper as
  # rustc-workspace-wrapper, so workspace crates still take tokens while deps
  # and vendor/aube do not. No in-tree caller produces this shape any more
  # (rust-build.sh blanks both keys or neither); it is the mark of a STALE
  # checkout's rust-build.sh, or a hand-rolled config.
  case "$w" in
    'RUSTC_WRAPPER=') mark='partial (workspace)'; partial=$((partial + 1)) ;;
    *)                mark='governed' ;;
  esac
  # A build the human marked foreground skips the slot queue by design; say
  # so rather than let it read as an escapee. (A build holding neither slot
  # nor ticket is otherwise NOT suspect — that is what idle reclaim looks like.)
  # rust-build.sh unsets NUB_BUILD_FG before its exec, so for that path the
  # tell is BOTH wrapper keys blank; the env var still catches a foreground
  # `make build` or a bare `cargo build`.
  if [ "$ww" = 'NUB_BUILD_FG=1' ] || { [ "$w" = 'RUSTC_WRAPPER=' ] && [ -n "$ww" ]; }; then
    mark='foreground (FG)'
  fi
  printf '  %-7s %8s  %-19s %s  [%s]\n' "$pid" "$etime" "$mark" \
    "$(printf '%s' "$rest" | sed 's|^[^ ]*/bin/||' | cut -c1-64)" "$(tree "$pid")"
done
[ "$partial" -gt 0 ] && printf '  %s build(s) partial: their dependency crates compile outside the cap\n' "$partial"

printf '\n'
