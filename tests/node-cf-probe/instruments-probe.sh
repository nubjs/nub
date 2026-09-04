#!/usr/bin/env bash
# The 2022 blocker for nodejs/node#44715, re-tested.
#
# bnoordhuis proposed dropping -framework CoreFoundation from the macOS link;
# evanlucas closed it with screenshots showing Instruments' Heap Allocations
# recorder producing no stacks without the framework, on macOS 12.6. This runs
# the same comparison headlessly on three control binaries -- CoreFoundation
# linked eagerly, linked delay-init, and not linked at all -- and reports
# whether the recorder still captures allocations and symbolicated backtraces.
#
# No Node build is involved: the question is about the loader state at launch,
# which a 30-line C++ program reproduces exactly.
set -uo pipefail
out=${1:-instruments-results}
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$out"; cd "$out"

sw_vers; DevToolsSecurity -status || true; xcrun xctrace version 2>&1 | head -2

cat > alloc.cc <<'EOF'
#include <CoreFoundation/CoreFoundation.h>
#include <cstdio>
#include <cstdlib>
#include <unistd.h>
#include <vector>

__attribute__((noinline)) static void* leaky_alloc(size_t n) { return std::malloc(n); }
__attribute__((noinline)) static void deep3(std::vector<void*>& v) {
  for (int i = 0; i < 2000; i++) v.push_back(leaky_alloc(4096));
}
__attribute__((noinline)) static void deep2(std::vector<void*>& v) { deep3(v); }
__attribute__((noinline)) static void deep1(std::vector<void*>& v) { deep2(v); }

int main(int argc, char** argv) {
  std::vector<void*> v;
  for (int round = 0; round < 40; round++) { deep1(v); usleep(20000); }
  // Never taken. Keeps the CoreFoundation imports live without loading it.
  if (argc > 99) {
    CFMutableArrayRef a =
        CFArrayCreateMutable(kCFAllocatorDefault, 0, &kCFTypeArrayCallBacks);
    if (a) CFRelease(a);
  }
  printf("allocated %zu blocks\n", v.size());
  return 0;
}
EOF

clang++ -O1 -g -mmacosx-version-min=13.5 -o alloc-eager alloc.cc -framework CoreFoundation
clang++ -O1 -g -mmacosx-version-min=13.5 -o alloc-delay alloc.cc -Wl,-delay_framework,CoreFoundation
clang++ -O1 -g -mmacosx-version-min=13.5 -o alloc-none  alloc.cc \
  -Wl,-U,_CFArrayCreateMutable -Wl,-U,_kCFAllocatorDefault \
  -Wl,-U,_kCFTypeArrayCallBacks -Wl,-U,_CFRelease

for b in alloc-eager alloc-delay alloc-none; do
  echo "### $b"
  otool -L "./$b" | sed 1d
  echo "  dyld initializers at launch: $(DYLD_PRINT_INITIALIZERS=1 "./$b" 2>&1 | grep -c 'running initializer')"
  echo "  CoreFoundation loaded:       $(DYLD_PRINT_LIBRARIES=1 "./$b" 2>&1 | grep -c 'Frameworks/CoreFoundation')"
done

for b in alloc-eager alloc-delay alloc-none; do
  echo "=== xctrace Allocations: $b"
  rm -rf "$b.trace"
  # --time-limit bounds the run without coreutils' timeout, which macOS lacks;
  # --no-prompt keeps the privacy warning from blocking a headless runner.
  xcrun xctrace record --template 'Allocations' --output "$b.trace" --no-prompt \
    --time-limit 60s --target-stdout "target-$b.txt" \
    --launch -- "./$b" > "xctrace-$b.log" 2>&1
  echo "  record exit=$?"
  sed 's/^/  | /' "xctrace-$b.log"
  [ -d "$b.trace" ] || { echo "  no trace produced"; continue; }
  xcrun xctrace export --input "$b.trace" --toc > "$b-toc.xml" 2>&1
  echo "  schemas:"
  grep -o 'schema="[^"]*"' "$b-toc.xml" | sort -u | sed 's/^/    /'
  for schema in $(grep -o 'schema="[^"]*"' "$b-toc.xml" | sed 's/schema="//;s/"//' | sort -u); do
    xcrun xctrace export --input "$b.trace" \
      --xpath "/trace-toc/run[@number=\"1\"]/data/table[@schema=\"$schema\"]" \
      > "$b-$schema.xml" 2>/dev/null
    rows=$(grep -c '<row>' "$b-$schema.xml" 2>/dev/null || echo 0)
    frames=$(grep -c '<frame ' "$b-$schema.xml" 2>/dev/null || echo 0)
    named=$(grep -o 'name="leaky_alloc[^"]*"' "$b-$schema.xml" 2>/dev/null | wc -l | tr -d ' ')
    echo "    $schema rows=$rows backtrace-frames=$frames leaky_alloc-frames=$named"
  done
done
