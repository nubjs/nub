#!/bin/sh
# shellcheck disable=SC2016,SC2329  # assertions are eval-ed expressions by design
# Exercises the build-slot layer of scripts/rustc-qos.sh without cargo, rustc
# or a loaded host: a fake `cargo` (a tiny C program that forks a shell script
# and waits — it must stay alive as an ANCESTOR, and it must be NAMED cargo,
# which a shell script cannot be on darwin where `ps comm` reports the
# interpreter) drives a fake `rustc` that only logs start/end and sleeps.
# Every scenario asserts on the ordering of those events, and the first one
# is the positive control: a second build must NOT start while the first
# compiles, so a refactor that fails the layer open goes red here.
#
#   tests/build-slots/run.sh            # all scenarios, ~2 min
#   tests/build-slots/run.sh fifo kill  # a subset
#
# POSIX sh; needs a C compiler on PATH (cc). State lives under a private
# NUB_BUILD_SEM_DIR so the machine's real governor is untouched.
set -u
here=$(cd "$(dirname "$0")" && pwd)
W="$here/../../scripts/rustc-qos.sh"
T=${TMPDIR:-/tmp}/build-slots-test.$$
mkdir -p "$T/bin" "$T/log"
trap 'pkill -f "$T" 2>/dev/null; rm -rf "$T"' EXIT INT TERM

cat > "$T/cargo.c" <<'C'
#include <unistd.h>
#include <sys/wait.h>
#include <stdlib.h>
int main(int argc, char **argv) {
  pid_t p = fork();
  if (p == 0) { argv[0] = "/bin/sh"; execv("/bin/sh", argv); _exit(127); }
  int st = 0; waitpid(p, &st, 0);
  return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}
C
cc -o "$T/bin/cargo" "$T/cargo.c" || { echo "SKIP: no C compiler"; exit 0; }
cp "$T/bin/cargo" "$T/bin/rust-analyzer"

# fake rustc: $1 build name, $2 seconds
cat > "$T/bin/rustc" <<RUSTC
#!/bin/sh
printf '%s %s start\n' "\$(date +%s)" "\$1" >> "$T/log/events"
sleep "\$2"
printf '%s %s end\n' "\$(date +%s)" "\$1" >> "$T/log/events"
RUSTC
chmod +x "$T/bin/rustc"

# fake build: name, compiles, parallelism, seconds per compile
cat > "$T/build.sh" <<BUILD
name=\$1; n=\$2; par=\$3; dur=\$4
printf '%s %s cargo-start\n' "\$(date +%s)" "\$name" >> "$T/log/events"
i=0
while [ \$i -lt \$n ]; do
  j=0
  while [ \$j -lt \$par ] && [ \$i -lt \$n ]; do
    "$W" "$T/bin/rustc" "\$name" "\$dur" 2>>"$T/log/wrapper.err" &
    j=\$((j+1)); i=\$((i+1))
  done
  wait
done
printf '%s %s cargo-end\n' "\$(date +%s)" "\$name" >> "$T/log/events"
BUILD

export NUB_BUILD_SEM_DIR="$T/sem" NUB_RUSTC_SEM_DIR="$T/rsem" NUB_BUILD_WAIT=40
unset NUB_BUILD_FG NUB_BUILD_SLOTS NUB_BUILD_IDLE NUB_BUILD_MAXCOMPILE
fails=0
# A scenario's leftovers (a build tree that outlived a kill) must not log into
# the next one's events, so reset takes down every fake process first.
reset() {
  pkill -9 -f "$T/build.sh" 2>/dev/null; pkill -9 -f "$T/bin/rustc" 2>/dev/null; sleep 1
  : > "$T/log/events"; rm -rf "$T/sem" "$T/rsem"
}
: > "$T/log/wrapper.err"
build() { "$T/bin/cargo" "$T/build.sh" "$@"; }
# seconds from the log's first event to the first / last "<name> <kind>" event
at() { awk -v n="$1" -v k="$2" 'NR==1{t0=$1} $2==n && $3==k {print $1-t0; exit}' "$T/log/events"; }
last() { awk -v n="$1" -v k="$2" 'NR==1{t0=$1} $2==n && $3==k {t=$1-t0} END{print t}' "$T/log/events"; }
check() {
  if eval "$2" 2>/dev/null; then echo "  ok   $1"; else echo "  FAIL $1"; fails=$((fails + 1)); fi
}
timeline() { awk 'NR==1{t0=$1} {printf "t+%ds %s %s; ", $1-t0, $2, $3}' "$T/log/events"; echo; }
want=" $* "
run() { [ "$want" = "  " ] || case $want in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

if run serialize; then
  echo "serialize: B must not start until A's cargo has exited (positive control)"
  reset; build A 4 2 3 & sleep 1; build B 2 2 2 & wait; timeline
  # Relational, not a wall-clock literal: with 4 compiles of 3s in pairs the
  # last start is ~3s after the first; over-throttled to one at a time it is
  # ~9s. The bound sits at the midpoint so a slow runner has 3s of slack.
  check "A's compiles ran in pairs (not over-throttled to one at a time)" '[ $(( $(last A start) - $(at A start) )) -le 6 ]'
  check "B did start" '[ -n "$(at B start)" ]'
  check "B started only after A ended" '[ "$(at B start)" -ge "$(at A cargo-end)" ]'
fi

if run fifo; then
  echo "fifo: three builds leave in the order they arrived"
  reset; build A 2 2 3 & sleep 1; build B 1 1 1 & sleep 1; build C 1 1 1 & wait; timeline
  check "B before C" '[ "$(at B start)" -lt "$(at C start)" ]'
  check "C after B ended" '[ "$(at C start)" -ge "$(at B end)" ]'
fi

if run kill; then
  echo "kill: a SIGKILLed holder releases the slot within seconds"
  reset; build K 6 2 3 & sleep 1; build B 1 1 1 & sleep 3
  # the whole build tree: the cargo shim (slot holder), its build script, its wrappers
  pkill -9 -f "$T/build.sh K " 2>/dev/null; pkill -9 -f "$T/bin/rustc K " 2>/dev/null
  wait 2>/dev/null; timeline
  check "B started within a few seconds of the kill" '[ "$(at B start)" -le 10 ]'
fi

if run idle; then
  echo "idle: a holder with no live compile for NUB_BUILD_IDLE yields; a holder mid-build is never robbed; a re-queue keeps its place"
  reset
  cat > "$T/idle.sh" <<IDLE
printf '%s A cargo-start\n' "\$(date +%s)" >> "$T/log/events"
"$W" "$T/bin/rustc" A 1 2>>"$T/log/wrapper.err"; sleep 9
"$W" "$T/bin/rustc" A 1 2>>"$T/log/wrapper.err"
printf '%s A cargo-end\n' "\$(date +%s)" >> "$T/log/events"
IDLE
  # A compiles 1s then idles 9s; B and C queue behind it; D queues after A has
  # re-queued, so A (first queued at t+1) must run before D but never interrupt C.
  # The window is 4s: C's between-compile gaps are ~0s, but a loaded host can
  # stretch one past 2s and a waiter taking the slot then is by design, not a bug.
  # A real steal costs C a whole other compile, which the +8 tolerance still sees.
  NUB_BUILD_IDLE=4 "$T/bin/cargo" "$T/idle.sh" & sleep 1
  NUB_BUILD_IDLE=4 build B 2 2 3 & sleep 1
  NUB_BUILD_IDLE=4 build C 3 1 3 & sleep 11
  NUB_BUILD_IDLE=4 build D 1 1 1 & wait; timeline
  check "B took the slot while A idled" '[ "$(at B start)" -lt 9 ]'
  # Correct: C's third start is ~6s after its first. A steal costs C a whole
  # extra turn (A's compile plus two polling gaps), so ~9s or more; 8 is the
  # midpoint-ish bound with 2s of slack for a slow runner.
  check "C's three compiles were never interrupted (3s apart, gaps under the window)" '[ "$(last C start)" -le $(( $(at C start) + 8 )) ]'
  check "A's second compile ran before D, not behind it" '[ "$(last A start)" -lt "$(at D start)" ]'
fi

if run nested; then
  echo "nested: an inner cargo inherits the outer build's slot instead of queueing behind it"
  reset
  cat > "$T/nested.sh" <<NESTED
printf '%s A cargo-start\n' "\$(date +%s)" >> "$T/log/events"
"$W" "$T/bin/rustc" A 1 2>>"$T/log/wrapper.err"
"$T/bin/cargo" "$T/build.sh" Ai 2 2 2
printf '%s A cargo-end\n' "\$(date +%s)" >> "$T/log/events"
NESTED
  "$T/bin/cargo" "$T/nested.sh" & sleep 1; build B 1 1 1 & wait; timeline
  check "inner ran while outer held" '[ "$(at Ai start)" -lt "$(at A cargo-end)" ]'
  check "B waited for the outer" '[ "$(at B start)" -ge "$(at A cargo-end)" ]'
fi

if run orphan; then
  echo "orphan: a queued wrapper killed while its cargo lives on must not wedge the head of the queue"
  reset
  cat > "$T/orphan.sh" <<ORPHAN
printf '%s O cargo-start\n' "\$(date +%s)" >> "$T/log/events"
"$W" "$T/bin/rustc" O 1 2>>"$T/log/wrapper.err" & w=\$!
sleep 2; kill -9 "\$w" 2>/dev/null
sleep 25
printf '%s O cargo-end\n' "\$(date +%s)" >> "$T/log/events"
ORPHAN
  build A 1 1 4 & sleep 1; "$T/bin/cargo" "$T/orphan.sh" & sleep 1; build B 1 1 1 & wait; timeline
  check "B ran before O's cargo exited (stale ticket ignored)" '[ "$(at B start)" -lt "$(at O cargo-end)" ]'
fi

if run probe; then
  echo "probe: rustc -vV never queues"
  reset; build A 1 1 4 & sleep 1
  s=$(date +%s); "$T/bin/cargo" -c "\"$W\" /bin/echo -vV >/dev/null 2>>\"$T/log/wrapper.err\""; e=$(( $(date +%s) - s )); wait
  check "probe returned immediately ($e s)" '[ "$e" -le 1 ]'
fi

if run exempt; then
  echo "exempt: NUB_BUILD_FG=1, rust-analyzer ancestry and NUB_BUILD_SLOTS=0 all run alongside a holder"
  reset; build A 1 1 5 & sleep 1
  NUB_BUILD_FG=1 build F 1 1 1 &
  "$T/bin/rust-analyzer" -c "\"$T/bin/cargo\" \"$T/build.sh\" R 1 1 1" &
  NUB_BUILD_SLOTS=0 build Z 1 1 1 & wait; timeline
  check "FG build overlapped A" '[ "$(at F start)" -lt "$(at A end)" ]'
  check "rust-analyzer build overlapped A" '[ "$(at R start)" -lt "$(at A end)" ]'
  check "slots=0 build overlapped A" '[ "$(at Z start)" -lt "$(at A end)" ]'
fi

# Independent of every scenario above: the wrapper sits in front of every rustc
# on the machine, so anything it writes to stderr lands in every cargo's output.
# Both stderr-noise bugs found on this script (`Illegal number`, `cannot open`)
# were invisible to the timeline assertions; this is the assertion that sees them.
echo "stderr: the wrapper wrote nothing to stderr across the whole run"
if [ -s "$T/log/wrapper.err" ]; then
  echo "  FAIL wrapper stderr ($(wc -l < "$T/log/wrapper.err" | tr -d ' ') lines):"; sed 's/^/       /' "$T/log/wrapper.err" | head -10
  fails=$((fails + 1))
else
  echo "  ok   wrapper stderr empty"
fi

echo
if [ "$fails" = 0 ]; then echo "build-slots: all scenarios passed"; else echo "build-slots: $fails assertion(s) FAILED"; fi
exit "$fails"
