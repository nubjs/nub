#!/usr/bin/env bash
# What delay-init MOVES rather than removes: the cost of the first call into
# CoreFoundation/Security, on real node binaries.
#
# The earlier round measured `--use-system-ca -e 0` and learned nothing: the
# off-thread keychain read is kicked off from lib/tls.js, which `-e 0` never
# loads, so that command touches Security exactly as much as plain `-e 0` does.
# Reach the path properly instead, in both of its shapes:
#
#   require("tls")                     starts the load on its own thread; the
#                                      main thread never waits for it
#   getCACertificates("system")        the main thread blocks on the result
#   fs.watch                           libuv dlopen()s CoreFoundation on the
#                                      main thread (deps/uv/src/unix/fsevents.c)
#
# usage: firstuse.sh <baseline-binary> <variant-binary> <outdir>
set -uo pipefail
base=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
var=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
out=$3; mkdir -p "$out"; cd "$out"
chmod +x "$base" "$var"

cat > watchy.js <<'JS'
const fs = require('fs');
const w = fs.watch(process.cwd(), () => {});
w.close();
JS
printf 'require("tls");\n' > tlsreq.js
printf 'console.log(require("tls").getCACertificates("system").length);\n' > sysca.js

sw_vers | sed -n 2p; uptime | sed 's/.*load/load/'

echo "########## the load really is deferred, and really does happen on demand"
# A count near 253 means the framework stayed off the launch path; a count near
# 659 on the variant means the first call pulled it in, which is the whole point.
for pair in "baseline:$base" "macos-cf:$var"; do
  label=${pair%%:*}; bin=${pair#*:}
  for cmd in "-e 0" "--use-system-ca sysca.js" "watchy.js"; do
    # shellcheck disable=SC2086
    n=$(DYLD_PRINT_INITIALIZERS=1 "$bin" $cmd 2>&1 >/dev/null | grep -c 'running initializer')
    printf 'initializers  %-9s %-26s %s\n' "$label" "$cmd" "$n"
  done
done

echo "########## same answer both ways"
echo "  baseline system roots: $("$base" --use-system-ca sysca.js)"
echo "  macos-cf system roots: $("$var" --use-system-ca sysca.js)"

echo "########## first-use cost, interleaved, min of 200"
run() {
  echo "--- $1"
  hyperfine -N --warmup 20 --runs 200 --export-json "$1.json" "$base $2" "$var $2" 2>&1 | sed 's/^/  /'
  uptime | sed 's/.*load/load/'
}
run e0            "-e 0"
run tls-require   "--use-system-ca tlsreq.js"
run system-ca     "--use-system-ca sysca.js"
run fs-watch      "watchy.js"
