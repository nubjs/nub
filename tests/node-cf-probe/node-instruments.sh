#!/usr/bin/env bash
# Run the Allocations recorder against a real node binary and report whether it
# captured allocations with symbolicated stacks. Same question as
# instruments-probe.sh, on the artifact the 2022 issue was actually about.
# usage: node-instruments.sh <node-binary> <label> <outdir>
set -uo pipefail
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1"); label=$2; out=$3
mkdir -p "$out"; cd "$out"
script=$(mktemp -t nodealloc).js
cat > "$script" <<'EOF'
const keep = [];
for (let round = 0; round < 40; round++) {
  for (let i = 0; i < 2000; i++) keep.push(Buffer.allocUnsafeSlow(4096));
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 20);
}
console.log('allocated', keep.length);
EOF

echo "### $label"
otool -L "$bin" | sed 1d
echo "  delay-init load commands: $(otool -l "$bin" | grep -c 'options delay-init')"
echo "  dyld initializers:        $(DYLD_PRINT_INITIALIZERS=1 "$bin" -e 0 2>&1 | grep -c 'running initializer')"
echo "  CoreFoundation loaded:    $(DYLD_PRINT_LIBRARIES=1 "$bin" -e 0 2>&1 | grep -c 'Frameworks/CoreFoundation')"
echo "  images loaded:            $(DYLD_PRINT_LIBRARIES=1 "$bin" -e 0 2>&1 | grep -c '^dyld\[[0-9]*\]: <')"
echo "  images delayed:           $(DYLD_PRINT_LIBRARIES=1 "$bin" -e 0 2>&1 | grep -c 'move loaded to delayed')"

echo "  --use-system-ca still works:"
"$bin" --use-system-ca -e 'const {rootCertificates}=require("tls");console.log("  system roots:",rootCertificates.length)' 2>&1 | sed 's/^/  | /'

rm -rf "trace-$label.trace"
# --time-limit bounds the run without coreutils' timeout, which macOS lacks;
# --no-prompt keeps the privacy warning from blocking a headless runner.
xcrun xctrace record --template 'Allocations' --output "trace-$label.trace" --no-prompt \
  --time-limit 120s --target-stdout "target-$label.txt" \
  --launch -- "$bin" "$script" > "xctrace-$label.log" 2>&1
echo "  record exit=$?"
sed 's/^/  | /' "xctrace-$label.log"
[ -d "trace-$label.trace" ] || { echo "  no trace produced"; exit 0; }
xcrun xctrace export --input "trace-$label.trace" --toc > "toc-$label.xml" 2>&1
for schema in $(grep -o 'schema="[^"]*"' "toc-$label.xml" | sed 's/schema="//;s/"//' | sort -u); do
  xcrun xctrace export --input "trace-$label.trace" \
    --xpath "/trace-toc/run[@number=\"1\"]/data/table[@schema=\"$schema\"]" \
    > "tbl-$label-$schema.xml" 2>/dev/null
  rows=$(grep -c '<row>' "tbl-$label-$schema.xml" 2>/dev/null || echo 0)
  frames=$(grep -c '<frame ' "tbl-$label-$schema.xml" 2>/dev/null || echo 0)
  echo "  $schema rows=$rows backtrace-frames=$frames"
done
