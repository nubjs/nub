#!/usr/bin/env bash
# Apply one build variant to a Node.js checkout.
#   baseline        nothing
#   pr65526         nodejs/node#65526 as-is: -fvisibility=hidden + -fvisibility-inlines-hidden for
#                   node, ada, icu, abseil (V8 already builds hidden since #56275)
#   inlines-hidden  the same files, but ONLY -fvisibility-inlines-hidden: every non-inline symbol
#                   stays exported, so addon/embedder compat is untouched; only the weak-def
#                   template/inline instantiations stop being coalesced by dyld at launch
#   atexit          nodejs/node#65549: drop the two atexit() registrations (ResetStdio, OpenSSL)
#                   and make ncrypto's well-known cipher table lazy
set -euo pipefail
variant=$1; dir=$2
here=$(cd "$(dirname "$0")" && pwd)
cd "$dir"
case "$variant" in
  baseline) ;;
  pr65526) git apply --3way "$here/patches/pr65526.patch" ;;
  inlines-hidden)
    git apply --3way "$here/patches/pr65526.patch"
    # Drop the two -fvisibility=hidden spellings (Xcode setting and cflag); keep inlines-hidden.
    for f in node.gyp deps/ada/ada.gyp tools/icu/icu-generic.gyp tools/v8_gypfiles/abseil.gyp; do
      sed -i '' "/GCC_SYMBOLS_PRIVATE_EXTERN/d; /'-fvisibility=hidden',/d" "$f"
    done
    ;;
  atexit) git apply --3way "$here/patches/pr65549.patch" ;;
  *) echo "unknown variant: $variant" >&2; exit 2 ;;
esac
echo "--- visibility flags now in node.gyp:"; grep -n 'visibility' node.gyp || true
git status --short | head -20
