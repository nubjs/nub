#!/usr/bin/env bash
# Apply one build variant to a Node.js checkout.
#   baseline        nothing
#   pr65526         nodejs/node#65526 as-is: -fvisibility=hidden + -fvisibility-inlines-hidden for
#                   node, ada, icu, abseil (V8 already builds hidden since #56275)
#   inlines-hidden  the same files, but ONLY -fvisibility-inlines-hidden: every non-inline symbol
#                   stays exported, so addon/embedder compat is untouched; only the weak-def
#                   template/inline instantiations stop being coalesced by dyld at launch
#   hasher          V8_USE_DEFAULT_HASHER_SECRET=1 for the V8 build: V8's own BUILD.gn default
#                   (Chrome's configuration), which Node's gyp build omits. Without it every
#                   isolate start runs rapidhash_make_secret(): a random search for three 64-bit
#                   primes with Miller-Rabin, ~12-14% of a hello-world's samples on Node >= 25.
#   atexit          the src/node.cc half of nodejs/node#65549: drop the two atexit()
#                   registrations (ResetStdio, OpenSSL). The lazy cipher-table half already
#                   landed on main, and each atexit() costs a dladdr() symbol-table scan on macOS.
set -euo pipefail
variant=$1; dir=$2
here=$(cd "$(dirname "$0")" && pwd)
cd "$dir"
case "$variant" in
  baseline) ;;
  pr65526) git apply "$here/patches/pr65526.patch" ;;
  inlines-hidden)
    git apply "$here/patches/pr65526.patch"
    # Drop the two -fvisibility=hidden spellings (Xcode setting and cflag); keep inlines-hidden.
    for f in node.gyp deps/ada/ada.gyp tools/icu/icu-generic.gyp tools/v8_gypfiles/abseil.gyp; do
      sed -i '' "/GCC_SYMBOLS_PRIVATE_EXTERN/d; /'-fvisibility=hidden',/d" "$f"
    done
    ;;
  hasher)
    python3 - <<'PY'
p = 'tools/v8_gypfiles/features.gypi'; s = open(p).read()
old = "    'defines': [\n      'V8_GYP_BUILD',\n"
assert old in s, 'anchor not found in features.gypi'
open(p, 'w').write(s.replace(old, old + "      'V8_USE_DEFAULT_HASHER_SECRET=1',\n", 1))
PY
    ;;
  atexit) git apply "$here/patches/atexit.patch" ;;
  *) echo "unknown variant: $variant" >&2; exit 2 ;;
esac
echo "--- visibility flags now in node.gyp:"; grep -n 'visibility' node.gyp || true
echo "--- hasher define:"; grep -n 'USE_DEFAULT_HASHER' tools/v8_gypfiles/features.gypi || true
git status --short | head -20
