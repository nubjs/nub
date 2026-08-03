#!/bin/sh
# timeout(1) for hosts that do not ship GNU coreutils — i.e. every macOS runner.
#
# ⛔ WHY THIS EXISTS. `timeout` is coreutils; the BSD userland on macOS does not have it and GitHub's
# macOS image does not add it. MEASURED on the first live macOS corpus slice: the fixture canary hit
# `run-batch.sh: line 146: timeout: command not found`, run-batch refused to run, and the job still
# went GREEN because the caller wraps it in `|| true` — 100 queue rows claimed, ZERO measured, and a
# commit that looked like progress.
#
# Dropping the cap instead of shimming it is not an option: one hung install would consume the whole
# job budget and take the other 99 measurements in the slice with it.
#
# Exits 124 on timeout, matching GNU semantics, because run-batch.sh reads 124 as HARNESS-TIMEOUT.
# Perl is present on macOS and every Linux runner, which is why it is the fallback rather than a
# bash-only construct.

secs="$1"
shift

exec perl -e '
  my $secs = shift @ARGV;
  my $pid  = fork();
  die "fork failed: $!" unless defined $pid;
  if ($pid == 0) {
    exec @ARGV;
    exit 127;                      # exec failed — same code a shell reports
  }
  # SIGKILL rather than SIGTERM: the thing being capped is a package install that may have spawned
  # its own children and may be ignoring TERM. A cap that can be ignored is not a cap.
  local $SIG{ALRM} = sub { kill "KILL", $pid; waitpid($pid, 0); exit 124; };
  alarm $secs;
  waitpid($pid, 0);
  alarm 0;
  exit($? >> 8);
' "$secs" "$@"
