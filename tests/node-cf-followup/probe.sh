#!/usr/bin/env bash
# Second-pass measurement of two already-built node binaries, so it costs a few
# minutes instead of a ~90-minute rebuild. It answers the one thing the first
# pass does not: what the first --use-system-ca or fs.watch call now costs, the
# load having moved there rather than disappeared.
#
# A node-level `leaks` and xctrace comparison was here and is deliberately gone.
# It ran past the job's 60-minute cap before reaching these measurements --
# `leaks --list` on 80,000 stack-logged allocations is enormous -- and it was
# redundant: tests/node-cf-instruments answers the recorder question on control
# binaries with a working positive control, and node-instruments.sh already
# records both node binaries in the build job.
# usage: probe.sh <baseline-binary> <variant-binary> <outdir>
set -uo pipefail
base=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
var=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
out=$3; mkdir -p "$out"; cd "$out"
chmod +x "$base" "$var"
printf 'console.log("hi")\n' > hello.js
cat > allocy.js <<'EOF'
const keep = [];
for (let i = 0; i < 80000; i++) keep.push(Buffer.allocUnsafeSlow(4096));
console.log('allocated', keep.length);
EOF

sw_vers; DevToolsSecurity -status || true; uptime | sed 's/.*load/load/'

for pair in "baseline:$base" "macos-cf:$var"; do
  label=${pair%%:*}; bin=${pair#*:}
  echo "########## $label"
  "$bin" --version
  otool -L "$bin" | sed 1d | sed 's/^/    /'
  echo "  LC_LOAD_DYLIB with delay-init: $(otool -l "$bin" | grep -c 'options delay-init')"
  echo "  dyld initializers, -e 0:       $(DYLD_PRINT_INITIALIZERS=1 "$bin" -e 0 2>&1 | grep -c 'running initializer')"
  echo "  ... of which node's own:       $(DYLD_PRINT_INITIALIZERS=1 "$bin" -e 0 2>&1 | grep -c 'running initializer.* in .*/node$')"
  echo "  CoreFoundation initializer:    $(DYLD_PRINT_INITIALIZERS=1 "$bin" -e 0 2>&1 | grep -c 'in /System/Library/Frameworks/CoreFoundation')"
  echo "  Network.framework inits:       $(DYLD_PRINT_INITIALIZERS=1 "$bin" -e 0 2>&1 | grep -c 'in /System/Library/Frameworks/Network')"
  echo "  initializers, --use-system-ca: $(DYLD_PRINT_INITIALIZERS=1 "$bin" --use-system-ca -e 'require("tls").rootCertificates' 2>&1 | grep -c 'running initializer')"
  echo "  system roots via --use-system-ca:"
  "$bin" --use-system-ca -e 'console.log("   ", require("tls").rootCertificates.length)' 2>&1 | sed 's/^/  | /'
done

echo "########## the cost delay-init moves rather than removes"
# --use-system-ca reads the keychain on its own thread, so the delayed load has
# main-thread bootstrap to overlap with. fs.watch does not: libuv dlopens
# CoreFoundation on the main thread, which is a refcount bump today and a real
# load under the patch. Measure both.
cat > watchy.js <<'EOF'
const fs = require('fs');
const w = fs.watch(process.cwd(), () => {});
w.close();
EOF
echo "--- and the plain launches again, interleaved, for a second reading"
hyperfine -N --warmup 20 --runs 300 --export-json ab-version.json \
  "$base --version" "$var --version"
hyperfine -N --warmup 20 --runs 300 --export-json ab-e0.json \
  "$base -e 0" "$var -e 0"

echo "--- the first-use paths"
hyperfine -N --warmup 10 --runs 200 --export-json ab-systemca.json \
  "$base --use-system-ca -e 0" "$var --use-system-ca -e 0"
hyperfine -N --warmup 10 --runs 200 --export-json ab-fswatch.json \
  "$base watchy.js" "$var watchy.js"
