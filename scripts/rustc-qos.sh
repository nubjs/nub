#!/bin/sh
# rustc-qos-version: 3  (build-status compares the installed copy against this)
# rustc-qos — machine-global cargo rustc-wrapper. Three jobs, all about stopping a
# fleet of concurrent agent builds from bricking a 10-core dev host:
#
#   1. QoS clamp (darwin): every rustc runs at 'utility', so builds always yield
#      to interactive work.
#   2. BUILD SLOTS: at most NUB_BUILD_SLOTS cargo invocations (default 1) may be
#      COMPILING at once, machine-wide, served first-come first-served. Every
#      other build's rustc waits at its first compile until a slot frees.
#   3. GLOBAL RUSTC SEMAPHORE: within the builds that hold a slot, at most N rustc
#      run machine-wide, across every worktree, every cargo, every entry point.
#
# WHY A BUILD-LEVEL CAP ON TOP OF THE RUSTC-LEVEL ONE. The rustc semaphore bounds
# how many rustc PROCESSES exist, and nothing else. Each holds a jobserver from
# its own cargo, so ten governed rustc still run up to `jobs` LLVM threads apiece
# (measured 2026-08-28: 10 rustc x 7 threads on 10 cores), and ten concurrent
# compiles of the big crates peak at 1-3 GiB each on a host already carrying
# ~40 GiB of editors, browsers and agent sessions — that is the swap storm the
# maintainer asked to end. Serialising at the BUILD level bounds both: one build
# at a time gets `jobs` threads and one build's worth of memory, and every other
# build waits in a queue instead of thrashing alongside it. It costs a little
# throughput (a sibling could have used the cores idle at one build's link or
# single-crate bottleneck) and buys a bounded memory peak and a build that
# finishes at full speed instead of N builds crawling.
#
# WHY THE CAPS LIVE HERE AND NOT IN CARGO'S JOB COUNT. Every per-build cap is
# blind to its siblings, so each cargo keeps its promise individually and the
# machine pays collectively: 13 concurrent agent builds x `jobs = 6` is a 78-way
# oversubscription of 10 cores (measured 2026-08-19 — load 464, 0% idle, 36% sys,
# 230 runnable threads, every one of those builds already on --profile fast). A
# rustc-wrapper is the only choke point that sees ALL of them, because cargo execs
# it once per codegen unit no matter who invoked cargo or from where. Capping here
# needs no agent discipline and no change to how builds are launched — which is
# what makes it hold, since the measured failure was precisely that 3 of the 13
# builds had bypassed scripts/rust-build.sh and its per-build clamp entirely.
#
# WHY BLOCKING CANNOT DEADLOCK. A slot belongs to the OUTERMOST cargo in this
# rustc's ancestry, so a nested cargo (a build script that runs cargo, `make` in
# front of cargo) inherits its parent's slot rather than queueing behind it. A
# slot-holding build proceeds to completion without waiting on anyone, so a
# queued build's wait is bounded by the builds ahead of it. Within a build, cargo
# spawns rustc only for units whose dependencies are already built, so any two
# concurrent rustc are independent by construction and one waiting can never
# block another's completion; a rustc token is held for the life of ONE rustc.
#
# WHY A SLOT CAN BE RECLAIMED FROM A LIVE CARGO. `cargo test` keeps running long
# after its last compile, and a cargo blocked on another target dir's lock never
# compiles at all. Holding the slot through either would stall every other build
# for nothing, so a slot is reclaimable once NO compile of its build is alive and
# none has started for NUB_BUILD_IDLE seconds. A build that then compiles again
# re-queues AT ITS ORIGINAL PLACE — its ticket carries the time it first queued
# — so losing the slot costs it one other build's turn, never a trip to the back
# of the line (measured 2026-08-28: a build that lost its slot in a build-script
# gap under load 40 re-queued behind four others for its last crate). Build
# scripts (cmake for zstd, ring's C) are the compile-adjacent gap that exceeds a
# short window, hence the 120s default.
#
# TWO LIVELOCK SHAPES, AND WHY NEITHER HOLDS. A queue ticket whose waiters all
# died while its cargo lives on would sit at the head forever, so every waiter
# heartbeats its ticket each loop and a ticket not refreshed for 15s is ignored
# and pruned. A live-compile marker whose pid was recycled by an unrelated
# long-lived process would pin its slot against idle reclaim, so a marker also
# records when it was written and one older than NUB_BUILD_MAXCOMPILE (30 min)
# no longer counts as live — no single compile here runs that long.
#
# FAIL-OPEN ON EVERY PATH. This sits in front of every rustc on the machine, so a
# bug here breaks every build in every worktree. No cargo ancestor, unwritable
# cache dir, a bad limit, exhausted retries: each falls through to running rustc
# unthrottled. rust-analyzer's `cargo check` is exempt from the build slots by
# design (it stays under the rustc semaphore) so the editor never queues behind
# the agent fleet.
#
# Installed by `make qos-global` into ~/.cargo (config.toml rustc-wrapper -> a
# stable copy at ~/.cargo/rustc-qos.sh). Deliberately NOT a tracked
# .cargo/config.toml: a sh wrapper there would break Windows (CI legs +
# contributors), and machine-global also covers stale worktrees and file://
# clones that predate any commit. Toggling the wrapper does not invalidate cargo
# fingerprints (verified 2026-07-24), so wrapped and unwrapped builds share a
# target dir without rebuild churn. NUB_BUILD_FG=1 is the HUMAN's foreground
# escape: it skips the QoS clamp and the build-slot queue (the rustc tokens still
# apply). It is for a person at a terminal, never for an agent build — that is
# how the 2026-08-19 oversubscription came about. `make build-status` shows the
# slots, the queue and the token pool.
#
# Tunables (all optional): NUB_BUILD_SLOTS (concurrent compiling builds, default
# 1; 0 disables the layer), NUB_BUILD_IDLE (seconds, default 120), NUB_BUILD_WAIT
# (queue ceiling in seconds, default 3600, then fail open),
# NUB_BUILD_MAXCOMPILE (seconds a compile marker stays live, default 1800),
# NUB_BUILD_SEM_DIR; NUB_RUSTC_LIMIT (concurrent rustc, default = ncpu),
# NUB_RUSTC_SEM_DIR, NUB_RUSTC_SEM_TRIES (retry ceiling).

# Cargo execs this wrapper for capability probes (`rustc -vV`, `--print …`) at
# startup and during build-script target detection. Those return in milliseconds
# and must never queue, or every cargo startup serializes behind the build fleet.
for _a in "$@"; do
  case $_a in
    -vV|--version|--print|--print=*) exec "$@" ;;
  esac
done

_qos=""
if [ "${NUB_BUILD_FG:-}" != "1" ] && [ "$(uname)" = "Darwin" ] \
  && command -v taskpolicy >/dev/null 2>&1; then
  _qos="taskpolicy -c utility"
fi

# RE-ENTRANCY. This script is installed as BOTH rustc-wrapper and
# rustc-workspace-wrapper, and cargo composes them for a workspace crate --
# outer wrapper, then inner. Without this guard each such rustc would take TWO
# tokens, and a pool fully held by outer wrappers all waiting on inner ones is a
# genuine deadlock. The marker is set only when a token is actually HELD, so an
# outer that failed open still lets the inner try.
if [ "${NUB_RUSTC_SEM_HELD:-}" = "1" ]; then
  exec "$@"
fi

_ncpu=$( { sysctl -n hw.ncpu || nproc; } 2>/dev/null || echo 4 )
_limit=${NUB_RUSTC_LIMIT:-$_ncpu}
_sem=${NUB_RUSTC_SEM_DIR:-$HOME/.cache/nub/rustc-sem}
_slot=""

# ---------------------------------------------------------------- build slots
_now() { date +%s; }
_bslots=${NUB_BUILD_SLOTS:-1}
_bidle=${NUB_BUILD_IDLE:-120}
_bwait=${NUB_BUILD_WAIT:-3600}
_bmax=${NUB_BUILD_MAXCOMPILE:-1800}
# A non-numeric tunable makes every [ … -gt "$var" ] below an error, which spews
# to the wrapper's real stderr and silently disables the guard it gates — for
# NUB_BUILD_WAIT that is the fail-open valve itself. Fall back to the default.
[ "$_bidle" -ge 0 ] 2>/dev/null || _bidle=120
[ "$_bwait" -ge 0 ] 2>/dev/null || _bwait=3600
[ "$_bmax" -ge 0 ] 2>/dev/null || _bmax=1800
_bdir=${NUB_BUILD_SEM_DIR:-$HOME/.cache/nub/build-sem}
_bslot=""
_cargo=""
_exempt=""

# One `ps -A` snapshot and an awk walk up from this shell: the OUTERMOST cargo is
# the build's identity (see the deadlock note above), and any rust-analyzer in
# the chain exempts the build. One fork, ~20ms, versus two forks per ancestor.
#
# The snapshot is the wrapper's one real cost (~190ms under load), and cargo
# execs this wrapper hundreds of times per build from the SAME parent, so the
# result is cached per parent pid. A recycled pid cannot alias: the entry also
# records the parent's start time and is discarded when that no longer matches.
_pcache=""
_walk=""
if [ "${_bslots:-0}" -gt 0 ] 2>/dev/null; then
  _pcache="$_bdir/parent/$PPID"
  _pstart=$(ps -o lstart= -p "$PPID" 2>/dev/null)
  if [ -n "$_pstart" ] && [ -r "$_pcache" ]; then
    { IFS= read -r _cstart && IFS= read -r _walk; } < "$_pcache" 2>/dev/null || _walk=""
    [ "$_cstart" = "$_pstart" ] || _walk=""
  fi
fi
if [ "${_bslots:-0}" -gt 0 ] 2>/dev/null && [ -z "$_walk" ]; then
  _walk=$(ps -Ao pid=,ppid=,comm= 2>/dev/null | awk -v start="$$" '
    { pp[$1] = $2; c = $3; for (i = 4; i <= NF; i++) c = c " " $i; comm[$1] = c }
    END {
      p = start; found = ""; ex = ""; n = 0
      while (p > 1 && n < 64 && (p in pp)) {
        if (comm[p] ~ /(^|\/)cargo$/) found = p
        if (comm[p] ~ /rust-analyzer/) ex = "ra"
        p = pp[p]; n++
      }
      print found, ex
    }')
  if [ -n "$_pstart" ] && mkdir -p "${_pcache%/*}" 2>/dev/null; then
    # A new parent is rare (once per cargo), so this is where dead entries go.
    for _e in "${_pcache%/*}"/*; do
      [ -e "$_e" ] && ! kill -0 "${_e##*/}" 2>/dev/null && rm -f "$_e" 2>/dev/null
    done
    printf '%s\n%s\n' "$_pstart" "$_walk" > "$_pcache.$$" 2>/dev/null \
      && mv "$_pcache.$$" "$_pcache" 2>/dev/null
  fi
fi
if [ "${_bslots:-0}" -gt 0 ] 2>/dev/null; then
  _cargo=${_walk%% *}
  _exempt=${_walk#* }
  [ "$_exempt" = "$_walk" ] && _exempt=""
  [ "${NUB_BUILD_FG:-}" = "1" ] && _exempt="fg"
fi

# Retire a slot dir ATOMICALLY: rename it out of the table first, so of two
# waiters that both judge one slot reclaimable exactly one wins the rename and
# the other's rm can never wipe a slot the winner has already re-created.
_retire() {
  _x=${1%/}
  _tomb="$_bdir/reap/${_x##*/}.$$"
  mv "$_x" "$_tomb" 2>/dev/null && rm -rf "$_tomb" 2>/dev/null
}

# Bring the slot table up to date: drop a slot whose cargo is gone, or whose
# build has no live compile and has not started one for NUB_BUILD_IDLE seconds;
# drop a queue ticket whose cargo is gone or whose waiters have stopped
# heartbeating. A compile is "live" while the pid in its marker is: on the exec
# path that pid IS the rustc, on the token-held path it is the shell that waits
# on it, so either way the marker outlives the compile by nothing and a SIGKILL
# leaves only a stale pid that `kill -0` rejects. The marker's content is the
# time it was written, so a recycled pid stops counting after NUB_BUILD_MAXCOMPILE.
_reap() {
  _tnow=$(_now)
  for _d in "$_bdir"/slot/*/; do
    [ -d "$_d" ] || continue
    _p=""; { read -r _p < "$_d/pid"; } 2>/dev/null || _p=""
    _s=""; { read -r _s < "$_d/stamp"; } 2>/dev/null || _s=""
    [ -n "$_s" ] || continue     # claimed this instant, stamp not yet written
    if [ -n "$_p" ] && ! kill -0 "$_p" 2>/dev/null; then
      _retire "$_d"; continue    # holder cargo is gone
    fi
    _live=0
    for _m in "$_d"active/*; do
      [ -e "$_m" ] || continue
      _w=0; { read -r _w < "$_m"; } 2>/dev/null || _w=0
      if kill -0 "${_m##*/}" 2>/dev/null && [ $(( _tnow - ${_w:-0} )) -le "$_bmax" ]; then
        _live=1
      else
        rm -f "$_m" 2>/dev/null
      fi
    done
    [ "$_live" = 1 ] && continue
    # No live compile: idle holder, or a claimer that died before writing its
    # pid. Either is reaped once the stamp is stale.
    if [ $(( _tnow - _s )) -gt "$_bidle" ]; then
      _retire "$_d"
    fi
  done
  for _t in "$_bdir"/queue/*; do
    [ -e "$_t" ] || continue
    _tp=${_t##*/}
    case $_tp in *.*) continue ;; esac   # a ticket mid-rename
    if ! kill -0 "$_tp" 2>/dev/null; then
      rm -f "$_t" 2>/dev/null; continue
    fi
    _hb=""; { read -r _hb; read -r _hb; } < "$_t" 2>/dev/null || _hb=""
    [ -n "$_hb" ] && [ $(( _tnow - _hb )) -gt 15 ] && rm -f "$_t" 2>/dev/null
  done
  # A build's first-queued record outlives its ticket by design (it is what a
  # re-queue reads), so it is collected here, by the death of its cargo — never
  # on the happy path, which would forfeit a re-queued build's place.
  for _f in "$_bdir"/first/*; do
    [ -e "$_f" ] || continue
    case ${_f##*/} in *.*) continue ;; esac
    kill -0 "${_f##*/}" 2>/dev/null || rm -f "$_f" 2>/dev/null
  done
}

# The slot this build already holds, if any.
_owned() {
  _i=1
  while [ "$_i" -le "$_bslots" ]; do
    _d="$_bdir/slot/$_i"
    _p=""; { read -r _p < "$_d/pid"; } 2>/dev/null || _p=""
    if [ "$_p" = "$_cargo" ]; then _bslot="$_d"; return 0; fi
    _i=$((_i + 1))
  done
  return 1
}

# Record this compile as live in $_bslot and refresh its stamp. Fails if the
# slot vanished underneath us (a sibling reaped it as idle a moment ago), in
# which case the caller keeps waiting rather than compiling unslotted.
_hold() {
  { [ -d "$_bslot/active" ] || mkdir -p "$_bslot/active" 2>/dev/null; } \
    && printf '%s\n' "$_tnow" > "$_bslot/active/$$" 2>/dev/null \
    && printf '%s\n' "$_tnow" > "$_bslot/stamp" 2>/dev/null \
    && [ -f "$_bslot/pid" ]
}

# Write or refresh this build's ticket: line 1 is when the build FIRST queued
# (kept across a re-queue, so a build that lost its slot keeps its place), line
# 2 is the heartbeat that says a waiter is still alive behind it.
_ticket() {
  # The record names the cargo's start time as well, so a pid recycled onto a
  # new build cannot inherit an old build's place — the same guard parent/ has.
  [ -n "${_cstart:-}" ] || _cstart=$(ps -o lstart= -p "$_cargo" 2>/dev/null)
  _f0=""; _fs=""
  { read -r _f0 && IFS= read -r _fs; } < "$_bdir/first/$_cargo" 2>/dev/null || _f0=""
  [ "$_fs" = "$_cstart" ] || _f0=""
  if [ -z "$_f0" ]; then
    _f0=$_tnow
    printf '%s\n%s\n' "$_f0" "$_cstart" > "$_bdir/first/$_cargo.$$" 2>/dev/null \
      && mv "$_bdir/first/$_cargo.$$" "$_bdir/first/$_cargo" 2>/dev/null
  fi
  printf '%s\n%s\n' "$_f0" "$_tnow" > "$_bdir/queue/$_cargo.$$" 2>/dev/null \
    && mv "$_bdir/queue/$_cargo.$$" "$_bdir/queue/$_cargo" 2>/dev/null
}

# First come, first served: this build may take a free slot only when its ticket
# is the oldest live one (first-queued time, then pid). Tickets are keyed by
# cargo pid, so every rustc of one build shares one place in line.
_head_of_queue() {
  _me=""; { read -r _me < "$_bdir/queue/$_cargo"; } 2>/dev/null || return 1
  [ -n "$_me" ] || return 1
  for _t in "$_bdir"/queue/*; do
    [ -e "$_t" ] || continue
    _op=${_t##*/}
    case $_op in *.*) continue ;; esac      # a ticket mid-rename
    [ "$_op" = "$_cargo" ] && continue
    _oe=""; { read -r _oe < "$_t"; } 2>/dev/null || continue
    [ -n "$_oe" ] || continue
    if [ "$_oe" -lt "$_me" ] || { [ "$_oe" -eq "$_me" ] && [ "$_op" -lt "$_cargo" ]; }; then
      return 1
    fi
  done
  return 0
}

if [ -n "$_cargo" ] && [ -z "$_exempt" ] \
  && mkdir -p "$_bdir/slot" "$_bdir/queue" "$_bdir/first" "$_bdir/reap" 2>/dev/null; then
  _t0=$(_now)
  _cstart=""
  while :; do
    _reap
    if _owned && _hold; then break; fi
    _bslot=""
    _ticket
    if _head_of_queue; then
      _i=1
      while [ "$_i" -le "$_bslots" ]; do
        _d="$_bdir/slot/$_i"
        if mkdir "$_d" 2>/dev/null; then
          # Stamp and live marker BEFORE the pid: a reaper judges a slot by
          # those, and a pid-bearing slot with neither would read as idle.
          printf '%s\n' "$_tnow" > "$_d/stamp" 2>/dev/null
          mkdir -p "$_d/active" 2>/dev/null && printf '%s\n' "$_tnow" > "$_d/active/$$" 2>/dev/null
          printf '%s\n' "$_cargo" > "$_d/pid" 2>/dev/null
          # Two rustc of one build can both be head of queue; the second to
          # claim would give the build two slots, so it yields the extra.
          _bslot=""
          if _owned && [ "$_bslot" = "$_d" ]; then break; fi
          _retire "$_d"; _bslot=""
          break
        fi
        _i=$((_i + 1))
      done
      [ -n "$_bslot" ] && break
    fi
    if [ $(( $(_now) - _t0 )) -ge "$_bwait" ]; then
      _bslot=""; break      # pathological wait: fail open, never stall a build
    fi
    sleep 1
  done
  rm -f "$_bdir/queue/$_cargo" 2>/dev/null
fi
# --------------------------------------------------------------- rustc tokens

# Retry ceiling x the sleep below bounds a wait at ~10 min. A rustc that waits
# that long has hit something pathological (a leaked token dir whose holder pid
# got recycled, say), so degrade to unthrottled rather than stall a build.
if [ "${_limit:-0}" -gt 0 ] 2>/dev/null && mkdir -p "$_sem" 2>/dev/null; then
  _tries=0
  while [ "$_tries" -lt "${NUB_RUSTC_SEM_TRIES:-1500}" ]; do
    _i=1
    while [ "$_i" -le "$_limit" ]; do
      _d="$_sem/$_i"
      # mkdir is the atomic test-and-set; the pid inside is only for reclaim.
      if mkdir "$_d" 2>/dev/null; then
        printf '%s\n' $$ > "$_d/pid" 2>/dev/null || true
        _slot="$_d"
        break
      fi
      # Reclaim a token whose holder died — SIGKILL skips the release trap, and
      # without this the pool would shrink monotonically to zero. `read` is a
      # builtin, so the contended path forks nothing but the sleep. An empty _p
      # means the holder has claimed but not yet written its pid: leave it be.
      # The brace group is load-bearing: a plain `read < missing 2>/dev/null`
      # still prints the REDIRECTION failure on the shell's own stderr, and this
      # races constantly (a holder that has mkdir'd but not yet written its pid).
      # That noise would interleave into every cargo's stderr machine-wide.
      _p=""
      if [ -r "$_d/pid" ]; then
        { read -r _p < "$_d/pid"; } 2>/dev/null || _p=""
      fi
      if [ -n "$_p" ] && ! kill -0 "$_p" 2>/dev/null; then
        rm -rf "$_d" 2>/dev/null || true
      fi
      _i=$((_i + 1))
    done
    [ -n "$_slot" ] && break
    _tries=$((_tries + 1))
    sleep 0.4
  done
fi

# Nothing held (exempt, or failed open): nothing to release, so exec and drop
# this shell.
if [ -z "$_slot" ] && [ -z "$_bslot" ]; then
  # shellcheck disable=SC2086  # $_qos word-splits deliberately (empty, or the clamp)
  exec $_qos "$@"
fi

# Holding a token or a slot means this shell must OUTLIVE rustc, so no exec: the
# token is released here, and the slot's stamp is refreshed at compile END as
# well as start. The end stamp is load-bearing: a compile longer than
# NUB_BUILD_IDLE would otherwise leave its build looking idle the instant it
# finished, and a waiter polling in the gap before cargo's next rustc would take
# the slot from a build in full flight (seen in the harness with a 3s compile
# under a 2s window; `aube` alone outlasts the 120s default under load).
if [ -n "$_slot" ]; then
  NUB_RUSTC_SEM_HELD=1
  export NUB_RUSTC_SEM_HELD
fi
# shellcheck disable=SC2329  # invoked from the traps below
_release() {
  [ -n "$_slot" ] && rm -rf "$_slot" 2>/dev/null
  if [ -n "$_bslot" ]; then
    rm -f "$_bslot/active/$$" 2>/dev/null
    [ -d "$_bslot" ] && printf '%s\n' "$(_now)" > "$_bslot/stamp" 2>/dev/null
  fi
  return 0
}
trap '_release' EXIT
# shellcheck disable=SC2086
$_qos "$@" &
_child=$!
# Forward termination, so killing the wrapper kills the rustc it owns. Without
# this the token would be released while the compile it guards still runs.
trap 'kill -TERM "$_child" 2>/dev/null; _release; exit 143' INT TERM
wait "$_child"
exit $?
