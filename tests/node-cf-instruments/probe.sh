#!/usr/bin/env bash
# Round 2 of the nodejs/node#44715 blocker re-test.
#
# Round 1 recorded all three control binaries under the Allocations template and
# got identical schema sets out of every trace -- but no malloc table in ANY of
# them, including the eager-CoreFoundation control, so it had no positive control
# and proved nothing. This round measures the underlying facility directly with
# `leaks`, which reports symbolicated malloc backtraces headlessly and therefore
# fails loudly when it captures nothing, and keeps the xctrace recording as a
# second reading with the trace bundle preserved.
set -uo pipefail
out=${1:-cf-instruments-results}
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

bins="alloc-eager alloc-delay alloc-none"

echo
echo "########## loader state at launch"
for b in $bins; do
  echo "### $b"
  otool -L "./$b" | sed 1d | sed 's/^/    /'
  echo "  LC_LOAD_DYLIB with delay-init: $(otool -l "./$b" | grep -c 'options delay-init')"
  echo "  dyld initializers:             $(DYLD_PRINT_INITIALIZERS=1 "./$b" 2>&1 | grep -c 'running initializer')"
  echo "  CoreFoundation initializer:    $(DYLD_PRINT_INITIALIZERS=1 "./$b" 2>&1 | grep -c 'in /System/Library/Frameworks/CoreFoundation')"
done

echo
echo "########## malloc stack logging via leaks (the facility the Heap Allocations recorder reads)"
echo "# alloc-eager is the positive control: a zero there means the instrument is broken,"
echo "# not that CoreFoundation matters."
for b in $bins; do
  MallocStackLogging=1 leaks --atExit --list -- "./$b" > "leaks-$b.txt" 2>&1
  echo "$b: exit=$? leaks_line=[$(grep -m1 'leaks for' "leaks-$b.txt")]"
  echo "    symbolicated leaky_alloc frames: $(grep -c 'leaky_alloc' "leaks-$b.txt")"
  echo "    symbolicated deep3 frames:       $(grep -c 'deep3' "leaks-$b.txt")"
  echo "    'Call stack' blocks:             $(grep -c 'Call stack' "leaks-$b.txt")"
done

echo
echo "########## Instruments Allocations recorder"
for b in $bins; do
  echo "### $b"
  rm -rf "$b.trace"
  xcrun xctrace record --template 'Allocations' --output "$b.trace" --no-prompt \
    --time-limit 60s --target-stdout "target-$b.txt" \
    --launch -- "./$b" > "xctrace-$b.log" 2>&1
  echo "  record exit=$?"
  sed 's/^/  | /' "xctrace-$b.log"
  [ -d "$b.trace" ] || { echo "  no trace produced"; continue; }
  echo "  trace bundle size:  $(du -sk "$b.trace" | cut -f1) KB"
  echo "  trace bundle files: $(find "$b.trace" -type f | wc -l | tr -d ' ')"
  find "$b.trace" -type f -size +16k -exec ls -l {} \; | awk '{print "    " $5, $9}' | sort -rn | head -12
  xcrun xctrace export --input "$b.trace" --toc > "$b-toc.xml" 2>&1
  echo "  toc schemas: $(grep -o 'schema="[^"]*"' "$b-toc.xml" | sort -u | tr -d '\n')"
done
