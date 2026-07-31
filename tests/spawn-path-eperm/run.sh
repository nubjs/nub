#!/bin/sh
# Reproduce the bare-name spawn EPERM under the real build jail.
#
#   tests/spawn-path-eperm/run.sh [path-to-nub] [tool]
#
# Builds a throwaway project whose `file:` dependency runs probe.js as its postinstall,
# so the probe executes inside the jail through the ordinary lifecycle path rather than a
# hand-written profile. Prints the probe lines; a `FAIL EPERM` on the verbatim arm beside
# an `OK` on the filtered arm is the defect.
set -eu

NUB=${1:-target/fast/nub}
TOOL=${2:-git}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# The side-effects cache replays a previous build and will silently null the run — a reused
# store key reports the OLD probe's output as if the script had just executed, which reads
# as "nothing changed". The nonce in the dependency VERSION is what forces a fresh key;
# `side-effects-cache=false` alone has been observed not to. It must vary with the tool as
# well as the clock: two runs in the same second otherwise collide and the second replays
# the first, reporting the wrong tool.
NONCE=$(date +%s)-$(echo "$TOOL" | tr -c 'a-zA-Z0-9' '-')-$$

mkdir -p "$WORK/dep" "$WORK/proj"
cp "$HERE/probe.js" "$WORK/dep/probe.js"
cat > "$WORK/dep/package.json" <<EOF
{ "name": "spawnprobe", "version": "1.0.0-$NONCE",
  "scripts": { "postinstall": "node ./probe.js" } }
EOF
cat > "$WORK/proj/package.json" <<'EOF'
{ "name": "spawnprobe-host", "version": "1.0.0",
  "dependencies": { "spawnprobe": "file:../dep" } }
EOF
printf 'side-effects-cache=false\n' > "$WORK/proj/.npmrc"

NUB_ABS=$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")
cd "$WORK/proj"
# The jail's env is a default-deny allowlist, so a custom variable does not survive into
# the child. Bake the tool name into the script instead of passing it through the env.
printf 'process.env.SPAWN_PROBE_TOOL = %s;\n' "\"$TOOL\"" > "$WORK/dep/tool.js"
sed -i.bak '1i\
require("./tool.js");
' "$WORK/dep/probe.js" && rm -f "$WORK/dep/probe.js.bak"

# The first install only RECORDS the dependency as an unreviewed build; `approve-builds`
# is what executes it. Both steps are needed, in this order.
"$NUB_ABS" install > "$WORK/out.log" 2>&1
"$NUB_ABS" approve-builds --all >> "$WORK/out.log" 2>&1
STATUS=$?
echo "install exit=$STATUS"
grep PROBE "$WORK/out.log" || { echo "no probe output — the script did not run:"; tail -20 "$WORK/out.log"; exit 1; }
