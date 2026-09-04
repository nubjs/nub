#!/usr/bin/env bash
# Windows leg: clang-cl (what Node >= 24 builds with) and cl, each mul_mod form. A losing candidate is a
# result, not a failure: the job fails only if the control does.
set -u; cd "$(dirname "$0")"
clang-cl --version | head -1; cl 2>&1 | head -1
try() { local label=$1; shift; rm -f "$label.exe"
  if "$@" > "$label.log" 2>&1 && [ -f "$label.exe" ]; then
    if ./"$label.exe" 2000 > "$label.run" 2>&1; then echo "$label: OK   $(grep -E 'candidate|speedup' "$label.run" | tr '\n' ' ')"
    else echo "$label: RUN FAIL rc=$?"; tail -3 "$label.run"; fi
  else echo "$label: BUILD FAIL"; grep -iE 'error|unresolved|warning' "$label.log" | head -4; fi; }
try clangcl-loop   clang-cl /nologo /O2 /EHsc /DMULMOD_LOOP   mulmod-probe.cc -Fe:clangcl-loop.exe
try clangcl-int128 clang-cl /nologo /O2 /EHsc /DMULMOD_INT128 mulmod-probe.cc -Fe:clangcl-int128.exe
try clangcl-intrin clang-cl /nologo /O2 /EHsc /DMULMOD_INTRIN mulmod-probe.cc -Fe:clangcl-intrin.exe
try cl-loop        cl /nologo /O2 /EHsc /DMULMOD_LOOP   mulmod-probe.cc -Fe:cl-loop.exe
try cl-intrin      cl /nologo /O2 /EHsc /DMULMOD_INTRIN mulmod-probe.cc -Fe:cl-intrin.exe
echo "--- does an int128 object carry a /DEFAULTLIB directive for compiler-rt?"
clang-cl /nologo /O2 /EHsc /DMULMOD_INT128 /c mulmod-probe.cc -Fo:int128.obj > /dev/null 2>&1 && strings -a int128.obj | grep -i 'defaultlib\|clang_rt' | head -5
rt=$(find "/c/Program Files/LLVM/lib" -iname 'clang_rt.builtins-x86_64.lib' 2>/dev/null | head -1); echo "compiler-rt builtins on the runner: ${rt:-none}"
[ -n "$rt" ] && try clangcl-int128-explicit-rt clang-cl /nologo /O2 /EHsc /DMULMOD_INT128 mulmod-probe.cc -Fe:clangcl-int128-explicit-rt.exe /link "$(cygpath -w "$rt")"
grep -q '^clangcl-loop: OK' <(try clangcl-loop clang-cl /nologo /O2 /EHsc /DMULMOD_LOOP mulmod-probe.cc -Fe:clangcl-loop.exe) || { echo "CONTROL FAILED"; exit 1; }
