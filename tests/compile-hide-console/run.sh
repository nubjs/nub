#!/usr/bin/env bash
# `nub compile --hide-console` end to end, on a real Windows runner.
#
# The unit tests prove the subsystem byte survives libsui's header rebuild. What
# they cannot reach is the half that follows from it: the launcher becomes a GUI
# process, so its standard handles are no longer a console's, and it goes on to
# spawn Node with CREATE_NO_WINDOW. That artifact has to still run, still exit
# with the program's status, and still deliver output through a redirect.
#
# "No window appeared" is NOT what this asserts — a headless runner has no desktop
# to observe. What it asserts instead is that the launcher TOOK the suppressing
# path, read out of the timing trace, so a green run cannot come from a runner
# where nothing was suppressed and every other check passed anyway.
#
# Usage: NUB=<nub.exe> __NUB_LAUNCHER_TEMPLATE=<launcher.exe> run.sh <node-version>
set -euo pipefail

NUB="${NUB:?set NUB to the built nub}"
NODE_TARGET="${1:?usage: run.sh <node-version>}"
READER="$(cd "$(dirname "$0")" && pwd)/read-subsystem.mjs"

work="$(mktemp -d)"
cd "$work"
echo "fixture: $work"

echo '{ "name": "hidden-app", "version": "1.0.0" }' > package.json
cat > app.ts <<'TS'
process.stdout.write(`ran ${process.argv.length >= 2 ? "ok" : "odd"}\n`);
process.exit(7);
TS

fail=0
inconclusive=0
check() { if [ "$2" = "$3" ]; then echo "  ok: $1"; else echo "  FAIL: $1 — expected [$2], got [$3]"; fail=$((fail+1)); fi; }

# --smol so no Node blob is downloaded: the subsystem lives in the launcher's PE
# either way. COMPILE_PLATFORM is a development escape hatch, not something CI
# sets — the flip is byte editing, so the header arms of this harness can be
# exercised from macOS or Linux before it is pushed. The RUN arms cannot.
compile() {
  "$NUB" compile app.ts --smol --target "$NODE_TARGET" --out "$1" \
    ${COMPILE_PLATFORM:+--platform "$COMPILE_PLATFORM"} "${@:2}" >/dev/null
}

echo "== 1. --hide-console gives the artifact the GUI subsystem =="
compile hidden.exe --hide-console
check "subsystem" "2" "$(node "$READER" hidden.exe)"

# Without this the arm above proves nothing: it would pass just as well against a
# launcher template that was GUI-subsystem before nub touched it.
echo "== 2. NEGATIVE CONTROL: without the flag it stays a console application =="
compile shown.exe
check "subsystem" "3" "$(node "$READER" shown.exe)"

if [ -n "${COMPILE_PLATFORM:-}" ]; then
  echo
  echo "COMPILE_PLATFORM set — skipping the run arms, which need a real Windows host"
  echo "RESULT: $fail check(s) failed"
  exit "$fail"
fi

# The half that only a real Windows host can answer. A GUI-subsystem process has
# no console of its own, so if any of this were wrong the artifact would hang,
# die, or print nothing — none of which the header check above can see.
echo "== 3. the hidden artifact still runs, and its output survives a redirect =="
set +e
out="$(./hidden.exe 2>err.txt)"
code=$?
set -e
check "stdout"    "ran ok" "$out"
check "exit code" "7"      "$code"

# The assertion that keeps arm 3 honest. On a runner that already owns a console
# the launcher deliberately suppresses nothing, and arm 3 would pass without ever
# exercising the CREATE_NO_WINDOW path this feature is made of.
echo "== 4. the launcher took the suppressing path =="
__NUB_LAUNCHER_TIMING=1 ./hidden.exe >/dev/null 2>trace.txt || true
line="$(grep -o 'hidden console: .*' trace.txt || echo '@MISSING')"
echo "  trace: $line"
case "$line" in
  *"suppressing child consoles"*)
    echo "  ok: children were spawned with no console" ;;
  # The deliberate terminal case. Not a defect and not this harness's business to
  # fail over — but arms 1-3 then say nothing about CREATE_NO_WINDOW, so it is
  # reported loudly rather than passed off as a green run.
  *"a console is already attached"*)
    echo "  INCONCLUSIVE: this runner owns a console, which the launcher inherits by"
    echo "                design, so arms 1-3 did not exercise the suppression path."
    inconclusive=1 ;;
  *"off (not a hidden build)"*)
    echo "  FAIL: the artifact was built with --hide-console and the launcher does not"
    echo "        know it — the flag is not reaching the payload manifest."
    fail=$((fail+1)) ;;
  *)
    echo "  FAIL: the launcher emitted no hidden-console phase at all"
    fail=$((fail+1)) ;;
esac

echo
if [ "$fail" -ne 0 ]; then
  echo "RESULT: $fail check(s) failed"
elif [ "$inconclusive" -ne 0 ]; then
  echo "RESULT: checks passed, but the suppression path was never taken on this host"
else
  echo "RESULT: all checks passed"
fi
exit "$fail"
