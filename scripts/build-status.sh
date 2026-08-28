#!/bin/sh
# build-status — one-screen answer to "why is this machine saturated?"
#
# Exists because the 2026-08-19 saturation was invisible from inside any single
# agent session: every build was individually well-behaved (all --profile fast,
# all QoS-clamped, all under `jobs = 6`) and the damage was purely their SUM.
# Nothing printed the sum. The row that matters most is `escaping the cap`: a
# build launched with RUSTC_WRAPPER blanked or a foreign wrapper is outside the
# global semaphore, and that is how oversubscription comes back.
set -u

# `ps | grep` rather than pgrep throughout (shellcheck SC2009): pgrep matches
# against a truncated comm on darwin and reported 0 rustc processes on this host
# while ps showed 40. An undercount here reads as "the machine is idle", which is
# the one wrong answer this script exists to prevent.
ncpu=$( { sysctl -n hw.ncpu || nproc; } 2>/dev/null || echo 4 )
sem=${NUB_RUSTC_SEM_DIR:-$HOME/.cache/nub/rustc-sem}
limit=${NUB_RUSTC_LIMIT:-$ncpu}

cargos=$(ps -Ao command= | grep -c '[b]in/cargo')
rustcs=$(ps -Ao command= | grep -c '[b]in/rustc')
wrappers=$(ps -Ao command= | grep -c '[r]ustc-qos.sh')
held=$(find "$sem" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' ')
runnable=$(ps -Ao state= | cut -c1 | grep -c R)

printf '\n== host ==\n'
printf '  cores %s   %s\n' "$ncpu" "$(uptime | sed 's/.*load averages*://')"
printf '  runnable threads %s   rustc %s   cargo %s\n' "$runnable" "$rustcs" "$cargos"
printf '  disk %s\n' "$(df -h /System/Volumes/Data 2>/dev/null | awk 'NR==2{print $4" free ("$5" used)"}')"

printf '\n== build slots (one build compiles at a time; the rest queue) ==\n'
bdir=${NUB_BUILD_SEM_DIR:-$HOME/.cache/nub/build-sem}
bslots=${NUB_BUILD_SLOTS:-1}
if [ -d "$bdir/slot" ]; then
  now=$(date +%s)
  for d in "$bdir"/slot/*/; do
    [ -d "$d" ] || continue
    p=$(cat "$d/pid" 2>/dev/null)
    st=$(cat "$d/stamp" 2>/dev/null || echo "$now")
    live=0
    for m in "$d"active/*; do [ -e "$m" ] && kill -0 "${m##*/}" 2>/dev/null && live=$((live + 1)); done
    printf '  slot %s  cargo %-7s %8s  %s compiling, last start %ss ago  %s\n' \
      "$(basename "$d")" "$p" "$(ps -o etime= -p "$p" 2>/dev/null | tr -d ' ')" "$live" "$((now - st))" \
      "$(ps -o command= -p "$p" 2>/dev/null | sed 's|^[^ ]*/bin/||' | cut -c1-60)"
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
    printf '  queued  cargo %-7s waiting %4ss  %s\n' "$q" "$((now - first))" \
      "$(ps -o command= -p "$q" 2>/dev/null | sed 's|^[^ ]*/bin/||' | cut -c1-60)"
  done
  printf '  %s/%s slots in use, %s queued\n' \
    "$(find "$bdir/slot" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' ')" "$bslots" "$n"
else
  printf '  NOT ACTIVE — run: make qos-global\n'
fi

# The layer holds only for builds that inherited THIS wrapper. Every stale
# checkout's qos-global.sh copies its own older rustc-qos.sh over the installed
# one on `make install-dev`, so compare versions here rather than assume.
want=$(sed -n 's/^# rustc-qos-version: \([0-9]*\).*/\1/p' "$(dirname "$0")/rustc-qos.sh" 2>/dev/null)
have=$(sed -n 's/^# rustc-qos-version: \([0-9]*\).*/\1/p' "$HOME/.cargo/rustc-qos.sh" 2>/dev/null)
if [ -n "$want" ] && [ "${have:-0}" -lt "$want" ] 2>/dev/null; then
  printf '  STALE WRAPPER: ~/.cargo/rustc-qos.sh is v%s, this tree ships v%s — builds since the downgrade are outside the slot layer; run: make qos-global\n' "${have:-0}" "$want"
fi

printf '\n== global rustc semaphore ==\n'
if [ -d "$sem" ]; then
  printf '  %s/%s tokens held   %s wrapper shells (held + waiting)\n' "$held" "$limit" "$wrappers"
else
  printf '  NOT ACTIVE — run: make qos-global\n'
fi

printf '\n== live cargo invocations ==\n'
ps -Ao pid=,etime=,command= | grep '[b]in/cargo' | while read -r pid etime rest; do
  # A build is governed only if it inherited the machine-global wrapper. An
  # explicitly EMPTY RUSTC_WRAPPER is cargo's documented "no wrapper", so that is
  # the tell for an escapee -- not an ABSENT variable, which means "inherit".
  w=$(ps eww -o command= -p "$pid" 2>/dev/null | tr ' ' '\n' \
      | grep '^RUSTC_WRAPPER=' | head -1)
  # An explicitly EMPTY RUSTC_WRAPPER is cargo's documented "no wrapper", so such
  # a build is outside the full semaphore -- but NOT ungoverned: qos-global also
  # registers the wrapper as rustc-workspace-wrapper, which those callers do not
  # blank, so workspace crates still take tokens. Deps and vendor/aube do not.
  case "$w" in
    'RUSTC_WRAPPER=') mark='partial (workspace)' ;;
    *)                mark='governed' ;;
  esac
  # A build the human marked foreground skips the slot queue by design; say
  # so rather than let it read as an escapee. (A build holding neither slot
  # nor ticket is otherwise NOT suspect — that is what idle reclaim looks like.)
  if ps eww -o command= -p "$pid" 2>/dev/null | tr ' ' '\n' | grep -qx 'NUB_BUILD_FG=1'; then
    mark='foreground (FG)'
  fi
  printf '  %-7s %8s  %-19s %s\n' "$pid" "$etime" "$mark" \
    "$(printf '%s' "$rest" | sed 's|^[^ ]*/bin/||' | cut -c1-72)"
done 2>/dev/null

printf '\n'
