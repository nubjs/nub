#!/bin/sh
# rustc-qos-version: 7  (build-status compares the installed copy against this)
# rustc-qos — machine-global cargo rustc-wrapper. Three jobs, all about stopping a
# fleet of concurrent agent builds from bricking a 10-core dev host:
#
#   1. QoS clamp (darwin): every rustc runs at 'utility', so builds always yield
#      to interactive work.
#   2. BUILD SLOTS: at most NUB_BUILD_SLOTS cargo invocations (default 2) may be
#      COMPILING at once, machine-wide, served first-come first-served. Every
#      other build's rustc waits at its first compile until a slot frees.
#   3. GLOBAL RUSTC SEMAPHORE: within the builds that hold a slot, at most
#      NUB_RUSTC_LIMIT rustc (default 6) run machine-wide, across every worktree,
#      every cargo, every entry point.
#
# WHY A BUILD-LEVEL CAP ON TOP OF THE RUSTC-LEVEL ONE. The rustc semaphore bounds
# how many rustc PROCESSES exist, and nothing else. Each holds a jobserver from
# its own cargo, so ten governed rustc still run up to `jobs` LLVM threads apiece
# (measured 2026-08-28: 10 rustc x 7 threads on 10 cores), and ten concurrent
# compiles of the big crates peak at 1-3 GiB each on a host already carrying
# ~40 GiB of editors, browsers and agent sessions — that is the swap storm the
# maintainer asked to end. Capping BUILDS bounds who competes: two builds share
# the six tokens (~12 GiB at the big crates' peak) and the rest wait in a queue
# instead of thrashing alongside them, so the builds that run finish at speed.
# Strict one-at-a-time was the first cut and idled nine cores behind a single
# starved compile while eight builds queued for 25 minutes; two slots over six
# tokens is the maintainer's pick (2026-08-28).
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
# slot-holding build waits on nothing but rustc tokens, each held for the life
# of ONE rustc, so a queued build's wait is bounded by the builds ahead of it.
# Within a build, cargo spawns rustc only for units whose dependencies are
# already built, so any two concurrent rustc are independent by construction and
# one waiting can never block another's completion.
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
# ONE ACCEPTED RACE. A reaper that scans a holder's markers (none live), then
# the holder's next compile marks the slot and proceeds, then the reaper
# retires it: that compile runs unslotted while another build claims the slot.
# Bounded to one compile, and the state converges (the holder re-queues at its
# place). Closing it would need a mutex shared by idle-retire and every
# compile start; the cap breach it allows is smaller than that mutex's cost.
#
# FAIL-OPEN ON EVERY PATH. This sits in front of every rustc on the machine, so a
# bug here breaks every build in every worktree. No cargo ancestor, a state dir
# that is missing or unwritable (at entry or mid-wait — a full disk, an operator's
# `rm -rf`), a cargo that died while its rustc queued, a bad tunable, exhausted
# retries: each falls through to running rustc unthrottled. A bad tunable takes
# its default, except NUB_BUILD_SLOTS=0 / NUB_RUSTC_LIMIT=0, which switch that
# layer off for the build. rust-analyzer's `cargo check` is exempt from the build
# slots by design (it stays under the rustc semaphore) so the editor never queues
# behind the agent fleet. `touch $NUB_BUILD_SEM_DIR/off` is the host-wide off
# switch: every wrapper checks it on entry and on every wait iteration, so it
# releases builds already queued (`make build-slots-off` / `build-slots-on`).
#
# THE BODY IS ONE COMPOUND COMMAND. sh reads a script incrementally, so an
# installer that copies over the live file (this repo's own did, until 2026-08-28,
# and every checkout that predates that still does) ends each running wrapper at
# the new EOF with exit 0 — cargo then records a unit as built that no rustc ever
# produced. Braced, the whole body is parsed before any of it runs.
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
# slots, the queue and the token pool; a build queued for 20s says so once on
# its cargo's stderr, so a silent `Compiling` line is never mistaken for a hang.
#
# Tunables (all optional, read per process): NUB_BUILD_SLOTS (concurrent
# compiling builds, default 2; 0 disables the layer), NUB_BUILD_IDLE (seconds,
# default 120), NUB_BUILD_WAIT (queue ceiling in seconds, default 3600, then fail
# open), NUB_BUILD_MAXCOMPILE (seconds a compile marker stays live, default
# 1800), NUB_BUILD_SEM_DIR; NUB_RUSTC_LIMIT (concurrent rustc, default 6),
# NUB_RUSTC_SEM_DIR, NUB_RUSTC_SEM_TRIES (retry ceiling, default 1500 x 0.4s).
{

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
# outer wrapper, then inner. The outer settles everything (slot, token, or the
# decision to fail open) and marks it, so the inner just execs; without the
# guard each such rustc would take TWO tokens, and a pool fully held by outer
# wrappers all waiting on inner ones is a genuine deadlock.
if [ "${NUB_RUSTC_SEM_HELD:-}" = "1" ]; then
  exec "$@"
fi

_limit=${NUB_RUSTC_LIMIT:-6}
_tries_max=${NUB_RUSTC_SEM_TRIES:-1500}
_sem=${NUB_RUSTC_SEM_DIR:-$HOME/.cache/nub/rustc-sem}
_slot=""

# ---------------------------------------------------------------- build slots
# stderr silenced: a wrapper SIGKILLed mid-`$(_now)` leaves `date` writing to
# a closed pipe, and where SIGPIPE is ignored (anything under a Node parent —
# the CI runner, an agent harness) that is a `Broken pipe` line in cargo's
# output rather than a silent death.
_now() { date +%s 2>/dev/null; }
_bslots=${NUB_BUILD_SLOTS:-2}
_bidle=${NUB_BUILD_IDLE:-120}
_bwait=${NUB_BUILD_WAIT:-3600}
_bmax=${NUB_BUILD_MAXCOMPILE:-1800}
# A non-numeric tunable makes every [ … -gt "$var" ] below an error, which spews
# to the wrapper's real stderr and silently disables the guard it gates — for
# NUB_BUILD_WAIT that is the fail-open valve itself. Fall back to the default.
[ "$_bslots" -ge 0 ] 2>/dev/null || _bslots=2
[ "$_bidle" -ge 0 ] 2>/dev/null || _bidle=120
[ "$_bwait" -ge 0 ] 2>/dev/null || _bwait=3600
[ "$_bmax" -ge 0 ] 2>/dev/null || _bmax=1800
[ "$_limit" -ge 0 ] 2>/dev/null || _limit=6
[ "$_tries_max" -ge 0 ] 2>/dev/null || _tries_max=1500
_bdir=${NUB_BUILD_SEM_DIR:-$HOME/.cache/nub/build-sem}
_bslot=""
_cargo=""
_exempt=""

# Collect a state directory's dead entries. An entry is named for the pid it
# belongs to and lives exactly as long as that pid; a `name.<writer>` is a
# temp file mid-rename, garbage once its writer is gone. Nothing here is aged.
_sweep() {
  for _e in "$1"/*; do
    [ -e "$_e" ] || continue
    _n=${_e##*/}; _n=${_n#claim.}
    case $_n in
      *.*) kill -0 "${_n##*.}" 2>/dev/null || rm -rf "$_e" 2>/dev/null ;;
      *)   kill -0 "$_n" 2>/dev/null || rm -rf "$_e" 2>/dev/null ;;
    esac
  done
}

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
if [ "$_bslots" -gt 0 ] && [ ! -e "$_bdir/off" ]; then
  _pcache="$_bdir/parent/$PPID"
  _pstart=$(ps -o lstart= -p "$PPID" 2>/dev/null)
  if [ -n "$_pstart" ] && [ -r "$_pcache" ]; then
    { IFS= read -r _pcached && IFS= read -r _walk; } 2>/dev/null < "$_pcache" || _walk=""
    [ "$_pcached" = "$_pstart" ] || _walk=""
  fi
  if [ -z "$_walk" ]; then
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
      _sweep "${_pcache%/*}"
      { printf '%s\n%s\n' "$_pstart" "$_walk" > "$_pcache.$$" \
        && mv "$_pcache.$$" "$_pcache"; } 2>/dev/null
    fi
  fi
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

# A slot's stamp and its live markers are written by rename, never in place: a
# reader that lands between a `>`'s truncate and its write sees an EMPTY file,
# and an empty stamp read as "stampless" retired a live slot (reproduced at a
# 30% torn-read rate on this host). The marker temp is dot-prefixed so the
# `active/*` scan never sees it. Both are silent on a directory that vanished.
_stamp() { { printf '%s\n' "$_tnow" > "$1/.stamp.$$" && mv "$1/.stamp.$$" "$1/stamp"; } 2>/dev/null; }
_mark()  { { printf '%s\n' "$_tnow" > "$1/active/.$$" && mv "$1/active/.$$" "$1/active/$$"; } 2>/dev/null; }

# Bring the slot table up to date: drop a slot whose cargo is gone, or whose
# build has no live compile and has not started one for NUB_BUILD_IDLE seconds;
# drop a queue ticket whose cargo is gone or whose waiters have stopped
# heartbeating. A compile is "live" while the pid in its marker is: that pid is
# the shell that waits on the rustc (a slot holder never execs), so the marker
# outlives the compile by nothing and a SIGKILL leaves only a stale pid that
# `kill -0` rejects. The marker's content is the time it was written, so a
# recycled pid stops counting after NUB_BUILD_MAXCOMPILE. Every number read from
# the table is validated first: a corrupt file must never be a fatal
# arithmetic error in the wrapper of every rustc on the machine.
_reap() {
  _tnow=$(_now)
  # A `date` that could not fork (load 400+, thousands of processes) yields "";
  # judging ages against it would retire everything. Skip this poll instead.
  [ "$_tnow" -ge 1 ] 2>/dev/null || return 0
  for _d in "$_bdir"/slot/*/; do
    [ -d "$_d" ] || continue
    _p=""; { read -r _p < "$_d/pid"; } 2>/dev/null || _p=""
    if [ -n "$_p" ] && ! kill -0 "$_p" 2>/dev/null; then
      _retire "$_d"; continue    # holder cargo is gone
    fi
    _live=0
    for _m in "$_d"active/*; do
      [ -e "$_m" ] || continue
      if kill -0 "${_m##*/}" 2>/dev/null; then
        # An unreadable age with a live pid counts as live, never as expired:
        # the fallback direction decides whether a torn read kills a compile.
        _w=""; { read -r _w < "$_m"; } 2>/dev/null || _w=""
        [ "$_w" -ge 0 ] 2>/dev/null || _w=$_tnow
        [ $(( _tnow - _w )) -le "$_bmax" ] && { _live=1; continue; }
      fi
      rm -f "$_m" 2>/dev/null
    done
    [ "$_live" = 1 ] && continue
    # No live marker. Read the stamp only now: a holder's _release refreshes
    # the stamp and THEN drops its marker, so a marker seen gone implies the
    # fresh stamp is already there.
    _s=""; { read -r _s < "$_d/stamp"; } 2>/dev/null || _s=""
    if ! [ "$_s" -ge 0 ] 2>/dev/null; then
      # No stamp and no live compile: a claim in progress (it lands within
      # milliseconds of the mkdir), or the corpse of a claimer killed or out of
      # disk before it could write one. Only age tells them apart; the dir's
      # mtime is its claim time (later writes land in files, not on the dir),
      # and BSD find's `-mmin +1` means two minutes or more.
      [ -n "$(find "$_d" -maxdepth 0 -mmin +1 2>/dev/null)" ] && _retire "$_d"
      continue
    fi
    if [ $(( _tnow - _s )) -gt "$_bidle" ]; then
      _retire "$_d"
    fi
  done
  for _t in "$_bdir"/queue/*; do
    [ -e "$_t" ] || continue
    _tp=${_t##*/}
    case $_tp in
      *.*) kill -0 "${_tp##*.}" 2>/dev/null || rm -f "$_t" 2>/dev/null; continue ;;
    esac
    if ! kill -0 "$_tp" 2>/dev/null; then
      rm -f "$_t" 2>/dev/null; continue
    fi
    _hb=""; { read -r _hb; read -r _hb; } 2>/dev/null < "$_t" || _hb=""
    [ "$_hb" -ge 0 ] 2>/dev/null && [ $(( _tnow - _hb )) -gt 15 ] && rm -f "$_t" 2>/dev/null
  done
  # A build's first-queued record outlives its ticket by design (it is what a
  # re-queue reads), so it is collected here, by the death of its cargo — never
  # on the happy path, which would forfeit a re-queued build's place. Same for
  # the said-once marker, a claim mutex whose holder died, and a retire tombstone.
  _sweep "$_bdir/first"
  _sweep "$_bdir/said"
  _sweep "$_bdir/reap"
  for _e in "$_bdir"/claim.*; do
    [ -d "$_e" ] || continue
    _n=${_e##*/}; _n=${_n#claim.}
    case $_n in
      *.*) kill -0 "${_n##*.}" 2>/dev/null || rm -rf "$_e" 2>/dev/null; continue ;;
    esac
    # Dead holder, or (a pid never written, ENOSPC) a dead cargo: either way
    # nobody will release it.
    _mp=""; { read -r _mp; } 2>/dev/null < "$_e/pid" || _mp=""
    if { [ -n "$_mp" ] && ! kill -0 "$_mp" 2>/dev/null; } \
      || { [ -z "$_mp" ] && ! kill -0 "$_n" 2>/dev/null; }; then
      mv "$_e" "$_e.$$" 2>/dev/null && rm -rf "$_e.$$" 2>/dev/null
    fi
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

# Record this compile as live in $_bslot and refresh its stamp. Returns 1 if the
# slot vanished underneath us (a sibling reaped it as idle a moment ago) or
# another build re-created it in that same gap — the caller keeps waiting rather
# than compiling unslotted — and 2 if the table is unwritable, which is fail-open.
# Never re-creates the slot: a reaped slot re-made here would carry a live marker
# and no pid, which nothing can retire and nothing can claim.
_hold() {
  [ -d "$_bslot/active" ] || return 1
  [ "$_tnow" -ge 1 ] 2>/dev/null || return 2
  if ! _mark "$_bslot" || ! _stamp "$_bslot"; then
    # A write that failed because the slot was retired under us is case 1,
    # not "unwritable": fail open only when the directory is still there.
    [ -d "$_bslot" ] || return 1
    return 2
  fi
  _p=""; { read -r _p < "$_bslot/pid"; } 2>/dev/null || _p=""
  [ "$_p" = "$_cargo" ] && return 0
  rm -f "$_bslot/active/$$" 2>/dev/null
  return 1
}

# Write or refresh this build's ticket: line 1 is when the build FIRST queued
# (kept across a re-queue, so a build that lost its slot keeps its place), line
# 2 is the heartbeat that says a waiter is still alive behind it. Fails only
# when the ticket cannot be written.
_ticket() {
  # The record names the cargo's start time as well, so a pid recycled onto a
  # new build cannot inherit an old build's place — the same guard parent/ has.
  [ -n "${_cstart:-}" ] || _cstart=$(ps -o lstart= -p "$_cargo" 2>/dev/null)
  _f0=""; _fs=""
  # `2>/dev/null` BEFORE the input redirect: redirections apply left to right,
  # so the other order opens the (usually absent) file with stderr still live
  # and prints `cannot open` into every cargo's output.
  { read -r _f0 && IFS= read -r _fs; } 2>/dev/null < "$_bdir/first/$_cargo" || _f0=""
  [ "$_fs" = "$_cstart" ] && [ "$_f0" -ge 0 ] 2>/dev/null || _f0=""
  if [ -z "$_f0" ]; then
    _f0=$_tnow
    { printf '%s\n%s\n' "$_f0" "$_cstart" > "$_bdir/first/$_cargo.$$" \
      && mv "$_bdir/first/$_cargo.$$" "$_bdir/first/$_cargo"; } 2>/dev/null
  fi
  { printf '%s\n%s\n' "$_f0" "$_tnow" > "$_bdir/queue/$_cargo.$$" \
    && mv "$_bdir/queue/$_cargo.$$" "$_bdir/queue/$_cargo"; } 2>/dev/null
}

# First come, first served: this build may take a free slot only when its ticket
# is the oldest live one (first-queued time, then pid). Tickets are keyed by
# cargo pid, so every rustc of one build shares one place in line.
_head_of_queue() {
  _me=""; { read -r _me < "$_bdir/queue/$_cargo"; } 2>/dev/null || return 1
  [ "$_me" -ge 0 ] 2>/dev/null || return 1
  for _t in "$_bdir"/queue/*; do
    [ -e "$_t" ] || continue
    _op=${_t##*/}
    case $_op in *.*) continue ;; esac      # a ticket mid-rename
    [ "$_op" = "$_cargo" ] && continue
    _oe=""; { read -r _oe < "$_t"; } 2>/dev/null || continue
    [ "$_oe" -ge 0 ] 2>/dev/null || continue
    if [ "$_oe" -lt "$_me" ] || { [ "$_oe" -eq "$_me" ] && [ "$_op" -lt "$_cargo" ]; }; then
      return 1
    fi
  done
  return 0
}

# Once per build, after 20s in the queue: the only sign a queued build gives is
# cargo's silent `Compiling` line, which an agent on a foreground timeout reads
# as a hang and relaunches — at the back of the line.
_say_queued() {
  [ $(( $(_now) - _t0 )) -ge 20 ] || return 0
  mkdir -p "$_bdir/said" 2>/dev/null && mkdir "$_bdir/said/$_cargo" 2>/dev/null || return 0
  _q=0; for _t in "$_bdir"/queue/*; do case ${_t##*/} in *.*) ;; *) [ -e "$_t" ] && _q=$((_q + 1)) ;; esac; done
  # shellcheck disable=SC2016  # the backticks are prose for the reader
  printf 'rustc-qos: this build is queued for a machine-wide build slot (%s builds waiting; `make build-status` shows the queue)\n' "$_q" >&2
}

if [ -n "$_cargo" ] && [ -z "$_exempt" ]; then
  _t0=$(_now)
  _cstart=""
  _open=""
  while :; do
    # Each of these is a fail-open exit, taken in place rather than after
    # NUB_BUILD_WAIT: the host-wide off switch, a table wiped or unwritable
    # underneath us, a cargo that died while this rustc queued.
    [ -e "$_bdir/off" ] && { _bslot=""; break; }
    { [ -d "$_bdir/slot" ] && [ -d "$_bdir/queue" ] && [ -d "$_bdir/first" ] && [ -d "$_bdir/reap" ]; } \
      || mkdir -p "$_bdir/slot" "$_bdir/queue" "$_bdir/first" "$_bdir/reap" 2>/dev/null \
      || { _bslot=""; break; }
    kill -0 "$_cargo" 2>/dev/null || { _bslot=""; break; }
    _reap
    if _owned; then
      _hold; _rc=$?
      [ "$_rc" = 0 ] && break
      [ "$_rc" = 2 ] && { _bslot=""; break; }
    fi
    _bslot=""
    _ticket || break
    if _head_of_queue; then
      # SIBLINGS OF ONE BUILD MUST NOT CLAIM CONCURRENTLY. Two parallel first
      # compiles share one ticket, so both are head of queue at once; without
      # this mutex each can claim a different slot and the build holds two —
      # observed with slots=2: the yield below is racy when neither sibling's
      # pid is visible yet, and a doubly-held build starves everyone else. The
      # critical section is microseconds; a sibling that loses it just waits a
      # loop iteration and then finds the slot _owned. A holder killed mid-claim
      # leaves a mutex whose pid is dead, reclaimed by _reap.
      _mx="$_bdir/claim.$_cargo"
      if mkdir "$_mx" 2>/dev/null; then
        { printf '%s\n' $$ > "$_mx/pid"; } 2>/dev/null
        _i=1
        while [ "$_i" -le "$_bslots" ]; do
          _d="$_bdir/slot/$_i"
          if mkdir "$_d" 2>/dev/null; then
            # Stamp and live marker BEFORE the pid: a reaper judges a slot by
            # those, and a pid-bearing slot with neither would read as idle. A
            # claim that cannot write is a claim on a table that cannot work.
            if ! { _stamp "$_d" && mkdir -p "$_d/active" 2>/dev/null && _mark "$_d" \
              && { printf '%s\n' "$_cargo" > "$_d/pid"; } 2>/dev/null; }; then
              _retire "$_d"; _bslot=""; _open=1; break
            fi
            # Backstop for the same hazard across a mutex reclaim: keep only
            # the lowest slot this build owns, yield any extra.
            _bslot=""
            if _owned && [ "$_bslot" = "$_d" ]; then break; fi
            _retire "$_d"; _bslot=""
            break
          fi
          _i=$((_i + 1))
        done
        rm -rf "$_mx" 2>/dev/null
      fi
      [ -n "$_bslot" ] && break
      [ -n "$_open" ] && { _bslot=""; break; }
    fi
    if [ $(( $(_now) - _t0 )) -ge "$_bwait" ]; then
      _bslot=""; break      # pathological wait: fail open, never stall a build
    fi
    _say_queued
    sleep 1
  done
  rm -f "$_bdir/queue/$_cargo" 2>/dev/null
fi
# --------------------------------------------------------------- rustc tokens

# Retry ceiling x the sleep below bounds a wait at ~10 min. A rustc that waits
# that long has hit something pathological (a leaked token dir whose holder pid
# got recycled, say), so degrade to unthrottled rather than stall a build.
if [ "$_limit" -gt 0 ] && mkdir -p "$_sem" 2>/dev/null; then
  _tries=0
  while [ "$_tries" -lt "$_tries_max" ]; do
    _i=1
    while [ "$_i" -le "$_limit" ]; do
      _d="$_sem/$_i"
      # mkdir is the atomic test-and-set; the pid inside is only for reclaim.
      if mkdir "$_d" 2>/dev/null; then
        { printf '%s\n' $$ > "$_d/pid"; } 2>/dev/null || true
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
      # Rename before rm, as _retire does: two waiters can judge one holder dead.
      _p=""
      if [ -r "$_d/pid" ]; then
        { read -r _p < "$_d/pid"; } 2>/dev/null || _p=""
      fi
      if [ -n "$_p" ] && ! kill -0 "$_p" 2>/dev/null; then
        mv "$_d" "$_sem/.reap.$_i.$$" 2>/dev/null && rm -rf "$_sem/.reap.$_i.$$" 2>/dev/null
      fi
      _i=$((_i + 1))
    done
    [ -n "$_slot" ] && break
    _tries=$((_tries + 1))
    sleep 0.4
  done
fi
# The inner wrapper (see RE-ENTRANCY) inherits this shell's decision — a slot
# and token held, a layer disabled, or a wait already failed open — rather
# than running the protocol again.
NUB_RUSTC_SEM_HELD=1
export NUB_RUSTC_SEM_HELD

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
# the slot from a build in full flight (the harness's idle scenario runs a
# compile longer than its window for exactly this; `aube` alone outlasts the
# 120s default under load).
# shellcheck disable=SC2329  # invoked from the traps below
_release() {
  [ -n "$_slot" ] && rm -rf "$_slot" 2>/dev/null
  if [ -n "$_bslot" ]; then
    # Stamp FIRST, marker second. A reaper scans the markers and reads the
    # stamp only once it finds none live, so a reaper that sees this marker
    # gone is guaranteed to see the fresh stamp. The other order let a waiter
    # polling in the same second retire a slot whose compile had just ended
    # (caught by the idle scenario under load).
    # Only OUR slot: the path may by now name a slot retired and re-claimed by
    # another build (a SIGKILLed cargo leaves its wrappers running), whose
    # stamp is not ours to refresh.
    _p=""; { read -r _p < "$_bslot/pid"; } 2>/dev/null || _p=""
    _tnow=$(_now)
    [ "$_p" = "$_cargo" ] && [ "$_tnow" -ge 1 ] 2>/dev/null && _stamp "$_bslot"
    rm -f "$_bslot/active/$$" 2>/dev/null
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

}
