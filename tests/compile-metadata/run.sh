#!/usr/bin/env bash
# `nub compile --metadata` end to end, on a real Windows runner.
#
# What the unit tests cannot reach: the CLI flag, the package.json defaults, and
# the PE that libsui finally writes are three separate layers, and only running
# the binary exercises all three. `verify_artifact` already refuses a compile
# whose version resource is unreachable, so a zero exit proves the resource is
# THERE — what is left, and what this asserts, is that it holds the right VALUES.
#
# Read back by read-versioninfo.mjs, which shares no code with nub. Using nub's
# own reader would only prove it agrees with itself.
#
# Usage: NUB=<nub.exe> __NUB_LAUNCHER_TEMPLATE=<launcher.exe> run.sh <node-version>
set -euo pipefail

NUB="${NUB:?set NUB to the built nub}"
NODE_TARGET="${1:?usage: run.sh <node-version>}"
READER="$(cd "$(dirname "$0")" && pwd)/read-versioninfo.mjs"

work="$(mktemp -d)"
cd "$work"
echo "fixture: $work"

mkdir -p rich bare
cat > rich/package.json <<'JSON'
{ "name": "acme-tool", "version": "2.5.1-rc.3", "description": "Does a thing", "author": { "name": "Acme Inc." } }
JSON
echo '{}' > bare/package.json
echo 'console.log("hi")' | tee rich/app.ts > bare/app.ts

fail=0
field() { node "$READER" "$1" | node -e '
  let s = ""; process.stdin.on("data", (d) => (s += d)).on("end", () => {
    const j = JSON.parse(s), k = process.argv[1];
    if (k === "@present") return console.log(String(j.present));
    if (k === "@fileVersion") return console.log(j.fileVersion.join("."));
    console.log(k in j.strings ? j.strings[k] : "@ABSENT");
  });' "$2"; }
check() { if [ "$2" = "$3" ]; then echo "  ok: $1"; else echo "  FAIL: $1 — expected [$2], got [$3]"; fail=$((fail+1)); fi; }

# --smol so no Node blob is downloaded: the version resource lives in the
# launcher's PE either way, and this keeps the check to a couple of seconds.
# COMPILE_PLATFORM is a development escape hatch, not something CI sets: the
# resource is written by byte-editing, so the whole harness can be exercised from
# a macOS or Linux host with COMPILE_PLATFORM=win32-x64 before it is pushed.
compile() {
  "$NUB" compile "$1" --smol --target "$NODE_TARGET" --out "$2" \
    ${COMPILE_PLATFORM:+--platform "$COMPILE_PLATFORM"} "${@:3}" >/dev/null
}

echo "== 1. the manifest supplies the defaults =="
compile rich/app.ts one.exe
check "present"          "true"           "$(field one.exe @present)"
check "ProductName"      "acme-tool"      "$(field one.exe ProductName)"
check "CompanyName"      "Acme Inc."      "$(field one.exe CompanyName)"
check "FileDescription"  "Does a thing"   "$(field one.exe FileDescription)"
check "OriginalFilename" "one.exe"        "$(field one.exe OriginalFilename)"
# The string keeps the prerelease tag; the four-u16 block has nowhere to put it.
check "FileVersion"      "2.5.1-rc.3"     "$(field one.exe FileVersion)"
check "numeric block"    "2.5.1.0"        "$(field one.exe @fileVersion)"

echo "== 2. --metadata overrides one field and drops another =="
compile rich/app.ts two.exe --metadata "ProductName=Renamed" --metadata "CompanyName="
check "overridden" "Renamed"  "$(field two.exe ProductName)"
check "dropped"    "@ABSENT"  "$(field two.exe CompanyName)"

# Without this the two above prove nothing: a Windows binary could carry a
# version resource for reasons of its own, and every assertion would pass.
echo "== 3. NEGATIVE CONTROL: an empty manifest earns no resource at all =="
compile bare/app.ts three.exe
check "no resource" "false" "$(field three.exe @present)"

echo
if [ "$fail" -eq 0 ]; then echo "RESULT: all checks passed"; else echo "RESULT: $fail check(s) failed"; fi
exit "$fail"
