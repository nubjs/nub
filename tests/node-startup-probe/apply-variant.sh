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
#   secretfast      keep the random rapidhash secrets (the hasher define above undoes Node's
#                   CVE-2026-21717 fix, so it is a measurement, not a candidate) but compute
#                   mul_mod in deps/v8/third_party/rapidhash-v8/secret.h with a 128-bit multiply
#                   instead of a 64-iteration shift-add loop: bit-identical secrets, ~17x faster.
#   entropy         src/node.cc: seed V8 from uv_random() instead of OpenSSL's DRBG, and run the
#                   eager CSPRNG seeding check only when FIPS is in effect, so the default provider
#                   is not constructed before v8Start.
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
  secretfast)
    python3 - <<'PY2'
p = 'deps/v8/third_party/rapidhash-v8/secret.h'; s = open(p).read()
head = "                                         unsigned long long m) {\n  unsigned long long r = 0;\n  while (b) {\n    if (b & 1) {\n      unsigned long long r2 = r + a;\n"
tail = "      a = a2 % m;\n    }\n  }\n  return r;\n}\n"
assert s.count(head) == 1 and s.count(tail) == 1, 'mul_mod anchors not found in secret.h'
s = s.replace(head, head.replace("{\n  unsigned long long r = 0;", "{\n#if defined(__SIZEOF_INT128__)\n  return static_cast<unsigned long long>(\n      (static_cast<__uint128_t>(a) * b) % m);\n#else\n  unsigned long long r = 0;", 1), 1)
s = s.replace(tail, tail.replace("  return r;\n}", "  return r;\n#endif\n}", 1), 1)
open(p, 'w').write(s)
PY2
    ;;
  entropy)
    python3 - <<'PY2'
p = 'src/node.cc'; s = open(p).read()
old = """    // Ensure CSPRNG is properly seeded.
    CHECK(ncrypto::CSPRNG(nullptr, 0));

    V8::SetEntropySource([](unsigned char* buffer, size_t length) {
      // V8 falls back to very weak entropy when this function fails
      // and /dev/urandom isn't available. That wouldn't be so bad if
      // the entropy was only used for Math.random() but it's also used for
      // hash table and address space layout randomization. Better to abort.
      CHECK(ncrypto::CSPRNG(buffer, length));
      return true;
    });
"""
new = """    // With FIPS in effect, confirm the CSPRNG is seeded before V8 starts: a
    // misconfigured FIPS provider makes RAND_status() fail forever, and an
    // abort here beats a hang at the first crypto call. Otherwise leave
    // OpenSSL's DRBG uninstantiated until crypto is actually used.
    if (ncrypto::isFipsEnabled()) {
      CHECK(ncrypto::CSPRNG(nullptr, 0));
    }

    V8::SetEntropySource([](unsigned char* buffer, size_t length) {
      // V8 seeds its hash tables, address space layout and Math.random()
      // from this, none of it cryptographic. Read the OS CSPRNG directly:
      // going through OpenSSL instantiates its DRBG and constructs the default
      // provider's algorithm tables on every startup. V8 falls back to very
      // weak entropy when this function fails, so abort instead.
      CHECK_EQ(uv_random(nullptr, nullptr, buffer, length, 0, nullptr), 0);
      return true;
    });
"""
assert s.count(old) == 1, 'entropy anchor not found in src/node.cc'
open(p, 'w').write(s.replace(old, new, 1))
PY2
    ;;
  *) echo "unknown variant: $variant" >&2; exit 2 ;;
esac
echo "--- visibility flags now in node.gyp:"; grep -n 'visibility' node.gyp || true
echo "--- hasher define:"; grep -n 'USE_DEFAULT_HASHER' tools/v8_gypfiles/features.gypi || true
echo "--- mul_mod / entropy:"; grep -n '__uint128_t' deps/v8/third_party/rapidhash-v8/secret.h || true; grep -n 'uv_random\|isFipsEnabled' src/node.cc || true
git status --short | head -20
