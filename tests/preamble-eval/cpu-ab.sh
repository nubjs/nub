#!/usr/bin/env bash
# Compare the WORK two commands do, on a machine that is not quiet.
#
#   tests/preamble-eval/cpu-ab.sh ./art-full ./art-ungate full ungate
#
# Wall clock on this repo's dev host has a ~1.4 ms floor and a shared CI runner
# measured 0.50, 0.81 and 1.16 ms on three consecutive jobs — which is larger than
# most of the startup terms anyone wants to separate, and is how three plausible
# wrong numbers got recorded before. Child CPU time (user+sys, via bash's `times`)
# ignores what other tenants do to the clock, and measured an 0.075 ms floor here
# against its own null control while the host sat at load 18.
#
# Two things make it work and the second is not optional:
#
#   * Each block runs inside its OWN subshell, because `times` reports the CALLING
#     shell's accumulated child CPU and a command substitution forks — so the runs
#     and the `times` that reads them have to be in the same shell.
#   * The arms alternate (A B / B A / A B …). CPU time still DRIFTS with host load:
#     two straight A-then-B passes over the same artifact differed by 0.90 ms.
#     Alternating cancels the drift; running the order both ways cancels any bias
#     the alternation itself introduces.
#
# ALWAYS run the null control first — the same command as both arms — and throw the
# comparison away if it does not come back near zero. That is the only check that
# catches a broken instrument rather than a real difference.
#
# READ IT AS WORK, NOT AS STARTUP. This is user+sys summed across threads, so it is
# an upper bound on the wall-clock win, not the same number. Quote a wall-clock
# figure only from a quiet-machine hyperfine run.
set -euo pipefail

block() { # block <n> <cmd…> -> seconds of child CPU
  local n="$1"; shift
  ( for ((i = 0; i < n; i++)); do "$@" >/dev/null 2>&1; done; times ) | tail -1 |
    awk '{ split($1, a, "m"); split($2, b, "m"); u = a[2]; s = b[2];
           sub("s", "", u); sub("s", "", s);
           printf "%.4f", (a[1] * 60 + u) + (b[1] * 60 + s) }'
}

PER="${PER:-25}"
BLOCKS="${BLOCKS:-8}"
A="${1:?usage: cpu-ab.sh <cmd-a> <cmd-b> [label-a] [label-b]}"
B="${2:?}"
LA="${3:-A}"
LB="${4:-B}"

sa=0
sb=0
for ((k = 0; k < BLOCKS; k++)); do
  if ((k % 2 == 0)); then
    x=$(block "$PER" $A); y=$(block "$PER" $B)
  else
    y=$(block "$PER" $B); x=$(block "$PER" $A)
  fi
  sa=$(awk -v a="$sa" -v b="$x" 'BEGIN { printf "%.4f", a + b }')
  sb=$(awk -v a="$sb" -v b="$y" 'BEGIN { printf "%.4f", a + b }')
done

awk -v a="$sa" -v b="$sb" -v n="$((PER * BLOCKS))" -v la="$LA" -v lb="$LB" 'BEGIN {
  printf "%-12s %8.3f ms/run\n%-12s %8.3f ms/run\n%-12s %+8.3f ms  (%s minus %s, %d runs each)\n",
    la, a * 1000 / n, lb, b * 1000 / n, "delta", (b - a) * 1000 / n, lb, la, n
}'
