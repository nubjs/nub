#!/usr/bin/env bash
# Second-pass measurement of two already-built node binaries, so it costs a few
# minutes instead of a ~90-minute rebuild. Answers three things the first pass
# did not: whether the Allocations recorder behaves the same against a real node
# with CoreFoundation delayed, what the first --use-system-ca call now costs
# (the load moves there rather than disappearing), and the interleaved A/B.
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
  echo "  malloc stack logging (leaks):"
  MallocStackLogging=1 leaks --atExit --list -- "$bin" allocy.js > "leaks-$label.txt" 2>&1
  echo "    $(grep -m1 'leaks for' "leaks-$label.txt")"
  echo "    Call stack blocks: $(grep -c 'Call stack' "leaks-$label.txt")"
  rm -rf "trace-$label.trace"
  xcrun xctrace record --template 'Allocations' --output "trace-$label.trace" --no-prompt \
    --time-limit 120s --target-stdout "target-$label.txt" \
    --launch -- "$bin" allocy.js > "xctrace-$label.log" 2>&1
  echo "  xctrace record exit=$? bundle=$(du -sk "trace-$label.trace" 2>/dev/null | cut -f1) KB"
  sed 's/^/  | /' "xctrace-$label.log"
done

echo "########## interleaved A/B"
for cmd in "--version" "-e 0" "hello.js"; do
  hyperfine -N --warmup 20 --runs 300 --export-json "ab-$(echo "$cmd" | tr -dc 'a-z0-9').json" \
    "$base $cmd" "$var $cmd"
done
echo "########## first --use-system-ca call (the cost delay-init moves, not removes)"
hyperfine -N --warmup 10 --runs 100 --export-json ab-systemca.json \
  "$base --use-system-ca -e 0" "$var --use-system-ca -e 0"
