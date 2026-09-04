// Standalone copy of the secret generation in V8's third_party/rapidhash-v8/secret.h (deps/v8 at
// nodejs/node 53234233acb). Answers, per compiler and OS: does a 128-bit mul_mod compile, link and
// produce the same secrets as the shipped shift-add loop, and how much faster is it?
//   -DMULMOD_LOOP    the shipped loop against itself (control)
//   -DMULMOD_INT128  (__uint128_t)a * b % m, what the V8 patch does under __SIZEOF_INT128__
//   -DMULMOD_INTRIN  _umul128 + _udiv128, the MSVC-style alternative
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <chrono>
#include <vector>
typedef unsigned long long u64;
#if defined(_MSC_VER)
#include <intrin.h>
#endif
static inline uint64_t rapid_mix(uint64_t A, uint64_t B) {
#if defined(__SIZEOF_INT128__)
  __uint128_t c = (__uint128_t)A * B; return (uint64_t)c ^ (uint64_t)(c >> 64);
#elif defined(_MSC_VER) && defined(_M_X64)
  uint64_t hi; uint64_t lo = _umul128(A, B, &hi); return lo ^ hi;
#else
#error "need a 64x64->128 multiply"
#endif
}
static inline uint64_t wyrand(uint64_t* seed) { *seed += 0x2d358dccaa6c78a5ull; return rapid_mix(*seed, *seed ^ 0x8bb84b93962eacc9ull); }
static inline int popcount64(uint64_t x) { int n = 0; while (x) { x &= x - 1; n++; } return n; }
static long g_mulmod_calls = 0, g_sprp_calls = 0, g_candidates = 0;

// --- shipped ---
static inline u64 mul_mod_loop(u64 a, u64 b, u64 m) {
  g_mulmod_calls++;
  u64 r = 0;
  while (b) {
    if (b & 1) { u64 r2 = r + a; if (r2 < r) r2 -= m; r = r2 % m; }
    b >>= 1;
    if (b) { u64 a2 = a + a; if (a2 < a) a2 -= m; a = a2 % m; }
  }
  return r;
}
// --- proposed: what the V8 patch does under #if defined(__SIZEOF_INT128__) ---
#if defined(__SIZEOF_INT128__)
static inline u64 mul_mod_128(u64 a, u64 b, u64 m) {
  g_mulmod_calls++;
  return (u64)(((__uint128_t)a * b) % m);
}
#endif
// --- alternative for a compiler without __int128 division: MSVC-style intrinsics ---
#if defined(_MSC_VER) && defined(_M_X64)
static inline u64 mul_mod_intrin(u64 a, u64 b, u64 m) {
  g_mulmod_calls++;
  u64 hi, rem; u64 lo = _umul128(a, b, &hi); _udiv128(hi, lo, m, &rem); return rem;
}
#endif
#if defined(MULMOD_INTRIN)
#define MULMOD_CANDIDATE mul_mod_intrin
#define MULMOD_NAME "intrin(_umul128/_udiv128)"
#elif defined(MULMOD_LOOP)
#define MULMOD_CANDIDATE mul_mod_loop
#define MULMOD_NAME "loop(control)"
#else
#define MULMOD_CANDIDATE mul_mod_128
#define MULMOD_NAME "int128(__uint128_t % m)"
#endif

template <u64 (*MM)(u64, u64, u64)>
struct Gen {
  static u64 pow_mod(u64 a, u64 b, u64 m) { u64 r = 1; while (b) { if (b & 1) r = MM(r, a, m); b >>= 1; if (b) a = MM(a, a, m); } return r; }
  static unsigned sprp(u64 n, u64 a) {
    g_sprp_calls++;
    u64 d = n - 1; unsigned char s = 0;
    while (!(d & 0xff)) { d >>= 8; s += 8; }
    if (!(d & 0xf)) { d >>= 4; s += 4; }
    if (!(d & 0x3)) { d >>= 2; s += 2; }
    if (!(d & 0x1)) { d >>= 1; s += 1; }
    u64 b = pow_mod(a, d, n);
    if ((b == 1) || (b == (n - 1))) return 1;
    for (unsigned char r = 1; r < s; r++) { b = MM(b, b, n); if (b <= 1) return 0; if (b == (n - 1)) return 1; }
    return 0;
  }
  static unsigned is_prime(u64 n) {
    if (n < 2 || !(n & 1)) return 0;
    if (n < 4) return 1;
    if (!sprp(n, 2)) return 0;
    if (n < 2047) return 1;
    for (u64 a : {3ull, 5ull, 7ull, 11ull, 13ull, 17ull, 19ull, 23ull, 29ull, 31ull, 37ull}) if (!sprp(n, a)) return 0;
    return 1;
  }
  static void make_secret(uint64_t seed, uint64_t* secret) {
    uint8_t c[] = {15,  23,  27,  29,  30,  39,  43,  45,  46,  51,  53,  54, 57,  58,  60,  71,  75,  77,  78,  83,  85,  86,  89,  90,
                   92,  99,  101, 102, 105, 106, 108, 113, 114, 116, 120, 135, 139, 141, 142, 147, 149, 150, 153, 154, 156, 163, 165, 166,
                   169, 170, 172, 177, 178, 180, 184, 195, 197, 198, 201, 202, 204, 209, 210, 212, 216, 225, 226, 228, 232, 240};
    for (size_t i = 0; i < 3; i++) {
      uint8_t ok;
      do {
        ok = 1; g_candidates++;
        secret[i] = 0;
        for (size_t j = 0; j < 64; j += 8) secret[i] |= (uint64_t)(c[wyrand(&seed) % sizeof(c)]) << j;
        if (secret[i] % 2 == 0) { ok = 0; continue; }
        for (size_t j = 0; j < i; j++) if (popcount64(secret[j] ^ secret[i]) != 32) { ok = 0; break; }
        if (ok && !is_prime(secret[i])) ok = 0;
      } while (!ok);
    }
  }
};

int main(int argc, char** argv) {
  int n = argc > 1 ? atoi(argv[1]) : 1000;
  std::vector<uint64_t> seeds(n); uint64_t s = 0x9e3779b97f4a7c15ull; for (int i = 0; i < n; i++) { s ^= s << 13; s ^= s >> 7; s ^= s << 17; seeds[i] = s; }
  std::vector<uint64_t> a(3 * n), b(3 * n);
  using clk = std::chrono::steady_clock;
  g_mulmod_calls = g_sprp_calls = g_candidates = 0;
  auto t0 = clk::now(); for (int i = 0; i < n; i++) Gen<mul_mod_loop>::make_secret(seeds[i], &a[3 * i]); auto t1 = clk::now();
  long mm_loop = g_mulmod_calls, sprp_loop = g_sprp_calls, cand = g_candidates;
  g_mulmod_calls = g_sprp_calls = g_candidates = 0;
  auto t2 = clk::now(); for (int i = 0; i < n; i++) Gen<MULMOD_CANDIDATE>::make_secret(seeds[i], &b[3 * i]); auto t3 = clk::now();
  int mismatch = 0; for (int i = 0; i < 3 * n; i++) if (a[i] != b[i]) mismatch++;
  double us_loop = std::chrono::duration<double, std::micro>(t1 - t0).count() / n;
  double us_128 = std::chrono::duration<double, std::micro>(t3 - t2).count() / n;
  printf("candidate=%s seeds=%d mismatching_words=%d\n", MULMOD_NAME, n, mismatch);
  printf("per make_secret: loop mul_mod %.1f us | candidate mul_mod %.1f us | speedup %.0fx\n", us_loop, us_128, us_loop / us_128);
  printf("per make_secret: candidates %.1f, sprp calls %.1f, mul_mod calls %.0f\n", (double)cand / n, (double)sprp_loop / n, (double)mm_loop / n);
  printf("sample seed %016llx -> %016llx %016llx %016llx\n", (u64)seeds[0], (u64)a[0], (u64)a[1], (u64)a[2]);
  return mismatch ? 1 : 0;
}
