#!/usr/bin/env bash
# Compare the WORK several commands do, on a machine that is not quiet.
#
#   tests/preamble-eval/cpu-ab.sh full=./art-full ungate=./art-ungate control=./art-full
#
# ALWAYS pass a CONTROL arm naming the same command as the first arm. Its gap from
# that arm is this run's error bar, and it is the only thing that tells you whether
# any other row means anything. That is not paranoia: an earlier version of this
# script reported an 0.075 ms control gap on its first pair and -0.420 ms on the
# same pair fifteen minutes later, which would have turned a 0.36 ms reading into a
# finding. Measure the floor in the SAME run as the effect, never once up front.
#
# Why CPU time. Wall clock on this repo's dev host has a ~1.4 ms floor, and three
# consecutive shared CI runners gave baseline spreads of 0.50, 0.81 and 1.16 ms —
# larger than most startup terms anyone wants to separate, and how several
# plausible wrong numbers got recorded. Child CPU time (user+sys, through bash's
# `times`) ignores what other tenants do to the clock.
#
# Why the rotation. CPU time still DRIFTS with host load: two straight A-then-B
# passes over one artifact differed by 0.90 ms. Arms therefore run in short blocks
# whose ORDER rotates every round, so no arm sits systematically early or late. Keep
# PER small — finer interleaving cancels faster drift; the run costs the same.
#
# Each block runs in its OWN subshell because `times` reports the CALLING shell's
# accumulated child CPU, and a command substitution forks — so the runs and the
# `times` that reads them have to live in the same shell.
#
# READ IT AS WORK, NOT AS STARTUP. user+sys summed across threads bounds the
# wall-clock win from above rather than equalling it. Quote a wall-clock number
# only from a quiet-machine hyperfine run.
set -euo pipefail

block() { # block <n> <cmd…> -> seconds of child CPU
  local n="$1"; shift
  ( for ((i = 0; i < n; i++)); do "$@" >/dev/null 2>&1; done; times ) | tail -1 |
    awk '{ split($1, a, "m"); split($2, b, "m"); u = a[2]; s = b[2];
           sub("s", "", u); sub("s", "", s);
           printf "%.4f", (a[1] * 60 + u) + (b[1] * 60 + s) }'
}

PER="${PER:-5}"
ROUNDS="${ROUNDS:-40}"
[ "$#" -ge 2 ] || { echo "usage: cpu-ab.sh <label>=<cmd> <label>=<cmd> [<label>=<cmd>…]" >&2; exit 2; }

labels=()
cmds=()
for spec in "$@"; do
  labels+=("${spec%%=*}")
  cmds+=("${spec#*=}")
done
n=${#cmds[@]}
sums=()
for ((i = 0; i < n; i++)); do sums+=(0); done

for ((r = 0; r < ROUNDS; r++)); do
  for ((j = 0; j < n; j++)); do
    i=$(((j + r) % n))
    x=$(block "$PER" ${cmds[$i]})
    sums[$i]=$(awk -v a="${sums[$i]}" -v b="$x" 'BEGIN { printf "%.4f", a + b }')
  done
done

total=$((PER * ROUNDS))
base=$(awk -v a="${sums[0]}" -v n="$total" 'BEGIN { printf "%.4f", a * 1000 / n }')
for ((i = 0; i < n; i++)); do
  awk -v s="${sums[$i]}" -v n="$total" -v l="${labels[$i]}" -v b="$base" 'BEGIN {
    v = s * 1000 / n
    printf "%-12s %8.3f ms/run   %+7.3f vs %s\n", l, v, v - b, "arm 1"
  }'
done
echo "($total runs per arm, blocks of $PER, order rotated each round)"
