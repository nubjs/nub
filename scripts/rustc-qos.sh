#!/bin/sh
# rustc-qos — machine-global cargo rustc-wrapper. Two jobs, both about stopping a
# fleet of concurrent agent builds from bricking a 10-core dev host:
#
#   1. QoS clamp (darwin): every rustc runs at 'utility', so builds always yield
#      to interactive work.
#   2. GLOBAL CONCURRENCY SEMAPHORE: at most N rustc run machine-wide, across
#      every worktree, every cargo, and every entry point.
#
# WHY THE SEMAPHORE LIVES HERE AND NOT IN CARGO'S JOB COUNT. Every per-build cap
# is blind to its siblings, so each cargo keeps its promise individually and the
# machine pays collectively: 13 concurrent agent builds x `jobs = 6` is a 78-way
# oversubscription of 10 cores (measured 2026-08-19 — load 464, 0% idle, 36% sys,
# 230 runnable threads, every one of those builds already on --profile fast). A
# rustc-wrapper is the only choke point that sees ALL of them, because cargo execs
# it once per codegen unit no matter who invoked cargo or from where. Capping here
# needs no agent discipline and no change to how builds are launched — which is
# what makes it hold, since the measured failure was precisely that 3 of the 13
# builds had bypassed scripts/rust-build.sh and its per-build clamp entirely.
#
# WHY BLOCKING CANNOT DEADLOCK. Cargo spawns rustc only for units whose
# dependencies are already built, so any two concurrent rustc are independent by
# construction and one waiting can never block another's completion. A token is
# held for the life of ONE rustc — seconds to a couple of minutes — so a wait is
# bounded by a single compile, never by a whole build.
#
# FAIL-OPEN ON EVERY PATH. This sits in front of every rustc on the machine, so a
# bug here breaks every build in every worktree. Unwritable cache dir, a bad
# limit, exhausted retries: each falls through to running rustc unthrottled.
#
# Installed by `make qos-global` into ~/.cargo (config.toml rustc-wrapper -> a
# stable copy at ~/.cargo/rustc-qos.sh). Deliberately NOT a tracked
# .cargo/config.toml: a sh wrapper there would break Windows (CI legs +
# contributors), and machine-global also covers stale worktrees and file://
# clones that predate any commit. Toggling the wrapper does not invalidate cargo
# fingerprints (verified 2026-07-24), so wrapped and unwrapped builds share a
# target dir without rebuild churn. NUB_BUILD_FG=1 opts out of the QoS clamp.
#
# Tunables (all optional): NUB_RUSTC_LIMIT (concurrent rustc, default = ncpu),
# NUB_RUSTC_SEM_DIR (token dir), NUB_RUSTC_SEM_TRIES (retry ceiling).

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

# Uncontended, or fail-open: nothing to release, so exec and drop this shell.
if [ -z "$_slot" ]; then
  # shellcheck disable=SC2086  # $_qos word-splits deliberately (empty, or the clamp)
  exec $_qos "$@"
fi

# Holding a token means this shell must OUTLIVE rustc to release it, so no exec.
NUB_RUSTC_SEM_HELD=1
export NUB_RUSTC_SEM_HELD
trap 'rm -rf "$_slot" 2>/dev/null' EXIT
# shellcheck disable=SC2086
$_qos "$@" &
_child=$!
# Forward termination, so killing the wrapper kills the rustc it owns. Without
# this the token would be released while the compile it guards still runs.
trap 'kill -TERM "$_child" 2>/dev/null; rm -rf "$_slot" 2>/dev/null; exit 143' INT TERM
wait "$_child"
exit $?
