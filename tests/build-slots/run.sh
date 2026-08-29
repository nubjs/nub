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
#   tests/build-slots/run.sh            # all scenarios, ~3 min
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
# A missing compiler is a skip on a dev box and a FAILURE in CI, where a skip
# would read as a green governor job with zero coverage.
cc -o "$T/bin/cargo" "$T/cargo.c" || { echo "SKIP: no C compiler"; [ -n "${CI:-}" ] && exit 1; exit 0; }
cp "$T/bin/cargo" "$T/bin/rust-analyzer"

# fake rustc: $1 build name, $2 seconds, $3 exit status (default 0). It shares
# the wrapper's stderr, so its own `date` is silenced: SIGKILLed mid-call under
# a parent that ignores SIGPIPE (the CI runner) it would print `Broken pipe`
# into the file the stderr assertion reads.
cat > "$T/bin/rustc" <<RUSTC
#!/bin/sh
printf '%s %s start\n' "\$(date +%s 2>/dev/null)" "\$1" >> "$T/log/events"
sleep "\$2"
printf '%s %s end\n' "\$(date +%s 2>/dev/null)" "\$1" >> "$T/log/events"
exit "\${3:-0}"
RUSTC
chmod +x "$T/bin/rustc"

# fake build: name, compiles, parallelism, seconds per compile, [seconds
# before the first compile — the cargo exists, and so holds its pid, before
# it queues]
cat > "$T/build.sh" <<BUILD
name=\$1; n=\$2; par=\$3; dur=\$4
printf '%s %s cargo-start\n' "\$(date +%s)" "\$name" >> "$T/log/events"
[ -z "\${5:-}" ] || sleep "\$5"
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

# The scenarios exercise the MECHANISM, so they pin one slot; the machine
# default is 2 (see rustc-qos.sh) and the exempt/disabled paths are tested
# explicitly below.
export NUB_BUILD_SEM_DIR="$T/sem" NUB_RUSTC_SEM_DIR="$T/rsem" NUB_BUILD_WAIT=40 NUB_BUILD_SLOTS=1
unset NUB_BUILD_FG NUB_BUILD_IDLE NUB_BUILD_MAXCOMPILE
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
  # B compiles 2s so that with no wrapper at all C (arriving 1s after B)
  # would start during B's compile: arrival order alone must not satisfy this.
  reset; build A 2 2 3 & sleep 1; build B 1 1 2 & sleep 1; build C 1 1 1 & wait; timeline
  check "B before C" '[ "$(at B start)" -lt "$(at C start)" ]'
  check "C after B ended" '[ "$(at C start)" -ge "$(at B end)" ]'
fi

if run kill; then
  echo "kill: a SIGKILLed holder releases the slot within seconds"
  reset; build K 6 2 3 & sleep 1; build B 1 1 1 & sleep 3
  # the whole build tree: the cargo shim (slot holder), its build script, its wrappers
  pkill -9 -f "$T/build.sh K " 2>/dev/null; pkill -9 -f "$T/bin/rustc K " 2>/dev/null
  wait 2>/dev/null; timeline
  # The kill lands at t+4; without dead-holder reclaim B waits for K's 6 compiles.
  check "B started within a few seconds of the kill" '[ "$(at B start)" -le 12 ]'
fi

if run idle; then
  echo "idle: a holder with no live compile for NUB_BUILD_IDLE yields; a holder mid-build is never robbed; a re-queue keeps its place"
  reset
  cat > "$T/idle.sh" <<IDLE
printf '%s A cargo-start\n' "\$(date +%s)" >> "$T/log/events"
"$W" "$T/bin/rustc" A 1 2>>"$T/log/wrapper.err"; sleep 8
"$W" "$T/bin/rustc" A 5 2>>"$T/log/wrapper.err"
printf '%s A cargo-end\n' "\$(date +%s)" >> "$T/log/events"
IDLE
  # C's cargo is launched FIRST (lowest pid) but compiles only from t+2; A
  # compiles 1s at t+0 and idles 8s; B queues at t+1, D at t+5. B reclaims the
  # slot at ~t+6 and holds it to ~t+12 (two 5s compiles in a pair), so A
  # re-queues (t+9) while B still holds: at B's release the line is A (first
  # queued t+0), C (t+2), D (t+5), and A must go first — unless a re-queue
  # loses its place, in which case the tie falls to C's lower pid. C's compiles
  # are 6s under a 4s window: a holder whose compile OUTLASTS the window is
  # exactly the build a stale start-stamp would expose. Mutation-checked:
  # dropping the end-stamp lets A steal from C (the +15 bound); ignoring live
  # markers, or skipping _hold's pid check, lets D steal (the D-after-C
  # check). A 3s compile caught none of these.
  NUB_BUILD_IDLE=4 build C 3 1 6 2 &
  NUB_BUILD_IDLE=4 "$T/bin/cargo" "$T/idle.sh" & sleep 1
  NUB_BUILD_IDLE=4 build B 2 2 5 & sleep 4
  NUB_BUILD_IDLE=4 build D 1 1 1 & wait; timeline
  # Relational: B may start once A's first compile is 4s idle, plus polling
  # (observed +5..6). Without idle reclaim B waits for A's cargo to exit, ~30s.
  check "B took the slot while A idled" '[ "$(at B start)" -le $(( $(at A end) + 4 + 5 )) ]'
  check "A's re-queue kept its place: its second compile ran before C's first" '[ "$(last A start)" -lt "$(at C start)" ]'
  # Correct: C's third start is 12s after its first. A steal costs C at least
  # D's compile plus polling; 15 leaves 3s of slack.
  check "C's three compiles were never interrupted (a compile longer than the window is not idle)" '[ "$(last C start)" -le $(( $(at C start) + 15 )) ]'
  check "D never ran before C's cargo exited" '[ "$(at D start)" -ge "$(at C cargo-end)" ]'
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
  # Bounded by A's 1s compile plus polling, not by cargo-end (which the inner's
  # synchronous return always precedes): a queued inner would sit until fail-open.
  check "inner ran while outer held" '[ "$(at Ai start)" -le $(( $(at A start) + 4 )) ]'
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

if run deadcargo; then
  echo "deadcargo: a wrapper whose cargo was killed while it queued fails open at once, not after NUB_BUILD_WAIT"
  reset; build A 1 1 8 & sleep 1; build Q 1 1 1 & sleep 2
  # Only Q's cargo shim: its build script and queued wrapper live on, orphaned.
  pkill -9 -f "^$T/bin/cargo $T/build.sh Q " 2>/dev/null
  wait 2>/dev/null; timeline
  check "Q's orphaned rustc ran before A ended (fail-open in place)" '[ "$(at Q start)" -lt "$(at A end)" ]'
fi

if run twoslots; then
  echo "twoslots: with two slots, a second build overlaps the first instead of queueing"
  reset
  # A's two parallel first compiles share one ticket and race the claim; the
  # per-build mutex must keep A on ONE slot so B can take the other. Without
  # it this scenario hangs B until A exits (observed before the mutex).
  NUB_BUILD_SLOTS=2 build A 2 2 4 & sleep 1; NUB_BUILD_SLOTS=2 build B 2 2 2 & wait; timeline
  check "B ran alongside A" '[ "$(at B start)" -lt "$(at A cargo-end)" ]'
  check "A's pair still ran in parallel" '[ "$(last A start)" -lt "$(at A end)" ]'
fi

if run tokens; then
  echo "tokens: NUB_RUSTC_LIMIT=1 serializes a slot holder's parallel compiles"
  reset; NUB_RUSTC_LIMIT=1 build A 4 2 2 & wait; timeline
  # In pairs the last start is ~2s after the first; one token at a time, ~6s.
  check "A's compiles ran one at a time" '[ $(( $(last A start) - $(at A start) )) -ge 5 ]'
fi

if run exit; then
  echo "exit: rustc's exit status reaches cargo on the held path and the exec path"
  reset
  "$T/bin/cargo" -c "\"$W\" \"$T/bin/rustc\" X 1 3 2>>\"$T/log/wrapper.err\"; echo \$? > \"$T/log/rc.held\""
  NUB_BUILD_SLOTS=0 NUB_RUSTC_LIMIT=0 "$T/bin/cargo" -c "\"$W\" \"$T/bin/rustc\" Y 1 3 2>>\"$T/log/wrapper.err\"; echo \$? > \"$T/log/rc.exec\""
  check "held path returned 3" '[ "$(cat "$T/log/rc.held")" = 3 ]'
  check "exec path returned 3" '[ "$(cat "$T/log/rc.exec")" = 3 ]'
fi

if run off; then
  echo "off: the host-wide off switch releases a build already queued"
  reset; build A 1 1 6 & sleep 1; build B 1 1 1 & sleep 2
  touch "$T/sem/off"; wait; timeline
  check "B started once the switch was set, before A ended" '[ "$(at B start)" -lt "$(at A end)" ]'
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
# The one deliberate line — the once-per-build "queued" notice — is exempt.
echo "stderr: the wrapper wrote nothing but its queued notice to stderr across the whole run"
noise=$(grep -v '^rustc-qos: ' "$T/log/wrapper.err")
if [ -n "$noise" ]; then
  echo "  FAIL wrapper stderr:"; printf '%s\n' "$noise" | sed 's/^/       /' | head -10
  fails=$((fails + 1))
else
  echo "  ok   wrapper stderr clean"
fi

echo
if [ "$fails" = 0 ]; then echo "build-slots: all scenarios passed"; else echo "build-slots: $fails assertion(s) FAILED"; fi
exit "$fails"
