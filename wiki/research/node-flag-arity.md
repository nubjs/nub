# Node flag arity — the exhaustive value-accepting set, for file-tier subject resolution

## TL;DR / verdict

- **Scope:** this table covers the Node/file tier only — the one tier whose grammar is open, evolving, and not nub's. The nub-owned tiers (`run`/script, `dlx`/package, bin) keep their full clap parse, which is correct for those closed grammars. Only the file tier needs the arity set plus an introspect fallback, because clap fails closed on a future Node flag.
- **The whole problem reduces to one question per token:** does this Node flag consume the next token as its value? If yes, skip the flag and the next token; if no, skip just the flag. The first remaining non-`-` token is the file subject. That consumes-a-value set is the entire deliverable.
- **The value-accepting set is small and stable.** Current Node (v26) has ~72 canonical value-accepting options plus ~9 value-accepting aliases (`-e`, `-r`, `-C`, `--loader`, …). 44 are present byte-identically across Node 18 / 20 / 22 / 24 / 26 — the entire core (`--require`, `--import`, `--eval`, `--conditions`, `--experimental-loader`, `--max-http-header-size`, and the `--cpu-prof-*`/`--heap-prof-*`/`--report-*`/`--test-*` value flags). Per-major churn is +3 to +10 additions and 0–2 removals, concentrated in niche `--experimental-*` / `--test-*` / snapshot / SEA flags, so the set is stable enough to hardcode.
- **Inline implies space.** For node-native value options, anything accepting `--flag=value` also accepts `--flag value` — the same code path in the parser. There is no inline-only node-native value option.
- **V8 passthrough flags (`--max-old-space-size`, `--stack-size`, the ~972 `--v8-options`) are the one exception, and it helps:** node's parser does not consume their value token. `--max-old-space-size=4096` works as one token; `--max-old-space-size 4096` is mislocated by node itself, which treats `4096` as the first positional and lets V8 error. For subject scanning, every V8 flag, boolean, NoOp, and unknown `--`-flag is zero-arity; only node-native value options consume a separate token.
- **Forward-compat:** node itself rejects truly-unknown flags (`bad option: --foo`). The only unknown-but-valid flag is one added in a Node newer than nubx's baked table, so the safe trigger is a target node newer than the baked baseline plus an unrecognized `-`-token, which falls back to the ~54 ms `node`-introspect slow path. That cannot silently mis-locate (walked below).
- **Recommendation:** bake a generated static const (the value set, the known zero-arity universe, and the baseline node version) and gate a per-invocation introspect fallback on `node.version > baseline`. Steady state is zero spawns, with the correctness backstop firing only on a node newer than the nub running it. The always-correct alternative — derive once per node binary and cache by mtime, mirroring `discover_node` — trades one spawn per new binary for eliminating the staleness reasoning. Both are minimal.

---

## 1. Scope: the file tier only (the tiers are asymmetric)

nubx resolves a raw argv to one of several tiers, and the grammars are not symmetric, so there is no single universal pre-scan:

- **nub-owned tiers** (`run`/script, `dlx`/package, bin) — nub defines those grammars and knows them completely. They use a full clap parse, which robustly signals both that an argv resolves to a tier and that it does not.
- **The Node/file tier** — Node's flags are open, evolving, and not nub's. This is the only tier where clap would fail closed on a future Node flag and break pass-through, so it uses the arity table below plus the introspect fallback.

This document enumerates the Node value-accepting flags for the file-tier subject scan. It does not govern the nub-owned tiers.

## 2. Ground truth: Node's own option parser

Authoritative source: `node/src/node_options.{h,cc}` + `node_options-inl.h`. Each flag is registered with `AddOption(name, help, &field, ...)`; the C++ **field type** determines the `OptionType`, and the parser branches on that type. The enum (`node_options.h`):

```cpp
enum OptionType {
  kNoOp,       // 0  — accepted, ignored (e.g. removed/deprecated flags kept for compat)
  kV8Option,   // 1  — passed through to V8; node does NOT consume a value
  kBoolean,    // 2  — no value
  kInteger,    // 3  — VALUE
  kUInteger,   // 4  — VALUE
  kString,     // 5  — VALUE
  kHostPort,   // 6  — VALUE (e.g. --inspect-port)
  kStringList, // 7  — VALUE, repeatable (e.g. --require, --import)
};
```

The value-consuming branch in `OptionsParser<Options>::Parse` (`node_options-inl.h`) is the load-bearing code:

```cpp
// equals only honored for double-dash names:  --foo=bar  splits; -e=x does NOT.
const std::string::size_type equals_index =
    arg[0] == '-' && arg[1] == '-' ? arg.find('=') : std::string::npos;
...
std::string value;
if (info.type != kBoolean && info.type != kNoOp && info.type != kV8Option) {
  if (equals_index != std::string::npos) {
    value = arg.substr(equals_index + 1);          // --flag=value  (inline)
    if (value.empty()) { missing_argument(); break; }
  } else {
    if (args.empty()) { missing_argument(); break; }
    value = args.pop_first();                       // --flag value  (consume NEXT token)
    if (!value.empty() && value[0] == '-') { missing_argument(); break; }  // next is a flag → error
  }
}
```

Three facts fall out of this, and they are the entire model:

1. **Only `kInteger`/`kUInteger`/`kString`/`kHostPort`/`kStringList` consume a value.** The `if` excludes `kBoolean`, `kNoOp`, and `kV8Option`.
2. **Inline and space are the same path.** Any node-native value option accepts both `--flag=value` and `--flag value`. The `=` form is recognized only for double-dash names; short aliases like `-e` reach the value option after alias expansion and support only the space form (`-e=x` → `bad option`, verified).
3. **A space-form value flag consumes exactly the next token, and only if it is non-`-`.** If the next token starts with `-`, or argv is empty, it is a requires-an-argument error — node never skips a flag to grab a later value.

Two more from the top of the loop:

```cpp
while (!args.empty() && errors->empty()) {
  if (args.first().size() <= 1 || args.first()[0] != '-') break;   // (a)
  const std::string arg = args.pop_first();
  if (arg == "--") { ...; break; }                                 // (b)
  ...
  if (it == options_.end()) { v8_args->push_back(arg); continue; } // (c)
```

- **(a)** A token that is exactly `-` (size 1), or does not start with `-`, ends option parsing and becomes the first positional — the subject. So `-` is the stdin subject, not a flag.
- **(b)** `--` ends option parsing; the next token is the subject, even if it starts with `-`.
- **(c)** An unknown flag is pushed to `v8_args` and the loop continues without consuming a value. Node defers to V8, which either accepts it (a real V8 flag) or rejects it (`bad option`), so node treats unknown flags as zero-arity.

## 3. The arity model the file-tier scanner needs

From §2, every token is exactly one of:

| Class | Consumes next token? | Members |
|---|---|---|
| **Node-native value option** (`kInteger/kUInteger/kString/kHostPort/kStringList`) and its value-aliases | **Yes** (space form; inline `=` self-contains) | the §4 table — ~72 options + ~9 aliases |
| **Eval flag** (`-e`/`--eval`, `-p`/`--print`) | **Yes**, and **terminates the file tier** — eval mode, no file subject follows | §5 |
| **Boolean / NoOp** node option | No | ~150 flags (`--watch`, `--check`, `--enable-source-maps`, …) |
| **V8 passthrough** (`kV8Option`) | **No** (see §6 — node never consumes the value; `=` form self-contains, space form is mislocated by node too) | ~972 `--v8-options` |
| **Unknown `--`-flag** | No, per node — but **ambiguous to a stale table** (§7) | n/a |
| **`--`** | ends flags; next token = subject | — |
| **`-`** | not a flag — it **is** the subject (stdin) | — |
| **first non-`-` token** | not a flag — it **is** the subject (file) | — |

Scan: walk argv; skip booleans/V8/unknown (one token); skip a value flag **plus its space-form value** (two tokens); stop at an eval flag (no subject), `--` (next is subject), `-` (subject), or the first bare token (subject).

## 4. The exhaustive value-accepting set

Harvested empirically from each Node's own authoritative metadata — `internalBinding('options').getCLIOptionsInfo()`, the same map `lib/internal/options.js` consumes, whose `type` is the `OptionType` enum — dumped across Node 18.20.4 / 20.19.0 / 22.15.0 / 24.14.0 / 26.2.0.

### Canonical value-accepting options — Node v26.2.0 (72)

`★` = present byte-identically in **all** of Node 18/20/22/24/26 (the 44-flag stable core).

| Flag | Type | Stable | | Flag | Type | Stable |
|---|---|:--:|---|---|---|:--:|
| `--allow-fs-read` | StringList |  | | `--report-filename` | String | ★ |
| `--allow-fs-write` | StringList |  | | `--report-signal` | String | ★ |
| `--build-sea` | String |  | | `--require` | StringList | ★ |
| `--build-snapshot-config` | String |  | | `--run` | String |  |
| `--conditions` | StringList | ★ | | `--secure-heap` | Integer | ★ |
| `--cpu-prof-dir` | String | ★ | | `--secure-heap-min` | Integer | ★ |
| `--cpu-prof-interval` | UInteger | ★ | | `--security-revert` | StringList | ★ |
| `--cpu-prof-name` | String | ★ | | `--snapshot-blob` | String | ★ |
| `--diagnostic-dir` | String | ★ | | `--stack-trace-limit` | Integer |  |
| `--disable-proto` | String | ★ | | `--test-concurrency` | UInteger | ★ |
| `--disable-warning` | StringList |  | | `--test-coverage-branches` | UInteger |  |
| `--dns-result-order` | String | ★ | | `--test-coverage-exclude` | StringList |  |
| `--env-file` | StringList |  | | `--test-coverage-functions` | UInteger |  |
| `--env-file-if-exists` | StringList |  | | `--test-coverage-include` | StringList |  |
| `--eval` | String | ★ | | `--test-coverage-lines` | UInteger |  |
| `--experimental-config-file` | String |  | | `--test-global-setup` | String |  |
| `--experimental-loader` | StringList | ★ | | `--test-isolation` | String |  |
| `--experimental-sea-config` | String |  | | `--test-name-pattern` | StringList | ★ |
| `--experimental-test-tag-filter` | StringList |  | | `--test-random-seed` | UInteger |  |
| `--heap-prof-dir` | String | ★ | | `--test-reporter` | StringList | ★ |
| `--heap-prof-interval` | UInteger | ★ | | `--test-reporter-destination` | StringList | ★ |
| `--heap-prof-name` | String | ★ | | `--test-rerun-failures` | String |  |
| `--heapsnapshot-near-heap-limit` | Integer | ★ | | `--test-shard` | String | ★ |
| `--heapsnapshot-signal` | String | ★ | | `--test-skip-pattern` | StringList |  |
| `--icu-data-dir` | String | ★ | | `--test-timeout` | UInteger |  |
| `--import` | StringList | ★ | | `--title` | String | ★ |
| `--input-type` | String | ★ | | `--tls-cipher-list` | String | ★ |
| `--inspect-port` | HostPort | ★ | | `--tls-keylog` | String | ★ |
| `--inspect-publish-uid` | String | ★ | | `--trace-event-categories` | String | ★ |
| `--localstorage-file` | String |  | | `--trace-event-file-pattern` | String | ★ |
| `--max-http-header-size` | UInteger | ★ | | `--trace-require-module` | String |  |
| `--max-old-space-size-percentage` | String |  | | `--unhandled-rejections` | String | ★ |
| `--network-family-autoselection-attempt-timeout` | UInteger |  | | `--use-largepages` | String | ★ |
| `--openssl-config` | String | ★ | | `--v8-pool-size` | Integer | ★ |
| `--redirect-warnings` | String | ★ | | `--watch-kill-signal` | String |  |
| `--report-dir` | String | ★ | | `--watch-path` | StringList | ★ |

### Value-accepting aliases — Node v26.2.0 (9)

These expand to a value option and so consume a token in space form. `-e`, `-r`, and `-C` are stable across all five majors.

| Alias | Expands to | Type |
|---|---|---|
| `-e` | `--eval` | String |
| `-r` | `--require` | StringList |
| `-C` | `--conditions` | StringList |
| `--loader` | `--experimental-loader` | StringList |
| `--debug-port` | `--inspect-port` | HostPort |
| `--report-directory` | `--report-dir` | String |
| `--security-reverts` | `--security-revert` | StringList |
| `--experimental-test-isolation` | `--test-isolation` | String (added 24) |
| `--experimental-default-config-file` | `--experimental-config-file` | String (added 26) |

Aliases keyed with `=` (`--inspect=`, `--inspect-brk=`, …) expand only in inline form and never consume a separate token, so they do not affect the space-form scan. Aliases keyed `--x <arg>` (`--print <arg>`) expand only when a non-`-` token follows — the `-p`/`--print` eval chain in §5.

## 5. Tier-changing / tricky flags (enumerated, not guessed)

- **`-e` / `--eval`** — `kString`, consumes the next token as the script source. No file subject follows: the tier is eval, not file. (`node -e 'console.log(2+2)'` → `4`, verified.)
- **`-p` / `--print`** — `--print` itself is `kBoolean`. `node -p '1+1'` → `2` works through an alias chain: `-p` → `--print`, then the parser's alias loop matches `--print <arg>` because a non-`-` token follows, expanding to `-pe` → `{--print, --eval}`, and `--eval` consumes the token. For the scanner: `-p`/`--print` followed by a non-`-` token is eval mode with no file subject; followed by a `-`-token or EOL it stays a bare boolean (REPL print mode).
- **`--`** — ends option parsing; the next token is the subject, even if it starts with `-`. Verified.
- **`-`** (single dash) — the subject, meaning stdin. Size 1 breaks the option loop, and `echo 'code' | node -` runs stdin. Not a flag, not a path.
- **Preload value flags** — `--require`/`-r`, `--import`, `--experimental-loader`/`--loader`, `--conditions`/`-C`, all StringList. Each consumes one value and is repeatable, with each occurrence consuming its own value, so repetition does not change the scan (verified `node -r a -r b`). They do not change the tier — a file subject still follows.
- **Directory/file value flags** — `--cpu-prof-dir`, `--heap-prof-dir`, `--diagnostic-dir`, `--redirect-warnings`, `--report-dir`/`--report-filename`, `--title`, `--snapshot-blob`, `--icu-data-dir`, `--env-file` — all plain `kString`/`kStringList`, consuming one value with no tier change. `--title` is `kString`, not a no-arg flag, so it consumes.
- **V8 value flags** — `--max-old-space-size`, `--stack-size`, `--max-semi-space-size`, … — `kV8Option`, which consume nothing from node's view (§6).

## 6. V8 passthrough flags — node consumes no value

`kV8Option` flags are pushed verbatim to `v8_args` and the loop continues without consuming a value. Verified on v26 and v20:

```
$ node --max-old-space-size=4096 s.js          # inline: one token → s.js runs
SUBJECT_RAN []
$ node --max-old-space-size 4096 s.js          # space form: node does NOT consume 4096
Error: Value for flag --max-old-space-size of type size_t is out of bounds ...
```

In the space form node treats `--max-old-space-size` as a valueless V8 flag, then `4096` (non-`-`) breaks the option loop and becomes the first positional, and V8, handed `--max-old-space-size` with no value, errors. This is real Node behavior on every major. So:

- **For subject scanning, all ~972 V8 flags are zero-arity** — treat them like booleans. If a user writes the broken space form, nubx mislocating `4096` mirrors what node does, which is correct pass-through parity.
- nubx does not need to enumerate the 972 V8 flags by name. Any flag not in the §4 value set is scanned as zero-arity, which is what node does too: V8 flags, booleans, and unknowns all consume zero tokens.

## 7. Forward-compat: the unknown-flag fallback, proven safe

The risk is `nubx --new-flag value subject.js`, where `--new-flag` is a value flag added in a Node newer than nubx's baked table. A stale table treats `--new-flag` as zero-arity and picks `value` as the subject, while the real newer node consumes `value` and the subject is `subject.js` — a mislocation. Walked against node's actual behavior:

1. **Truly-unknown flags are not a hazard.** Node itself rejects a flag it does not know: `node --totally-bogus s.js` → `bad option: --totally-bogus` (verified). A flag unknown to both nubx and the target node is moot, because node errors regardless of where nubx thought the subject was.
2. **The only hazard is unknown-to-nubx, known-to-the-target-node** — a value flag added between nubx's baked baseline and the target node version. That is detectable: nubx already knows the exact node it is about to invoke (`discover_node`), and therefore its version.
3. **Safe trigger:** if the target node version is newer than the baked baseline and the argv contains a `-`-token nubx's table does not recognize, fall back to the ~54 ms `node`-introspect slow path. The introspect runs the real node, whose `getCLIOptionsInfo()` and own parse know the new flag, so it cannot mislocate.
4. **Why this cannot silently mis-locate:** the only path to a wrong subject is an unrecognized value flag consumed by the real node but not by nubx. Either the flag is unknown to the target node too, so node errors and there is no silent wrong run, or it is new in a newer node and the version gate plus unrecognized-token check routes to the authoritative introspect. There is no third case: a flag nubx recognizes is scanned with the correct arity by construction.

The fallback fires only when running a node newer than the nub, with a flag that nub predates, and only when such an unrecognized token is present. Steady state: zero fallbacks.

## 8. Stability / churn across majors

Value-accepting **canonical option** counts and deltas (excludes aliases), measured from the per-version dumps:

| Transition | Added | Removed | Notes |
|---|---|---|---|
| 18.20 → 20.19 | +10 | −1 | added `--env-file`, `--allow-fs-read/write`, `--disable-warning`, `--test-timeout`, SEA/snapshot; removed `--experimental-specifier-resolution` |
| 20.19 → 22.15 | +10 | −2 | added `--run`, `--stack-trace-limit`, `--localstorage-file`, the `--test-coverage-*` family; removed `--experimental-policy`, `--policy-integrity` |
| 22.15 → 24.14 | +6 | −2 | added `--experimental-config-file`, `--test-global-setup`, `--watch-kill-signal`; removed `--experimental-default-type`, `--experimental-test-isolation` (→ renamed `--test-isolation`) |
| 24.14 → 26.2 | +3 | 0 | added `--build-sea`, `--experimental-test-tag-filter`, `--test-random-seed` |

Totals: 48 → 57 → 65 → 69 → 72, so the per-major delta is +3 to +10 additions and 0–2 removals. Every addition is a niche `--experimental-*` / `--test-*` / diagnostic / SEA-snapshot flag, none of which realistically appears on a `nubx`/file-run command line, and every removal is an experimental feature graduating or being dropped. The 44-flag core — every preload, eval, conditions, profiling, report, TLS, and HTTP flag — is byte-stable across 18→26. The load-bearing set is effectively frozen; churn lives in the long tail.

## 9. Recommendation: bake vs derive-and-cache

Two viable shapes, both minimal:

**(A) Bake a generated static const plus a version-gated introspect fallback — recommended primary.**
- Generate, at nub build time from the latest Node's `getCLIOptionsInfo()`, a const carrying the value-accepting set (§4), the known zero-arity universe (booleans + V8 + NoOp, names only), and the baseline Node version.
- Scan offline with zero spawns. Per §7, fall back to the introspect slow path only when `target_node.version > baseline` and an unrecognized `-`-token is present.
- **Pro:** zero-cost in the steady state and fully offline. **Con:** the const is regenerated each nub release, and correctness for newer-than-nub nodes rides entirely on the fallback, which is sound.

**(B) Derive once per node binary and cache by mtime — robust alternative.**
- On the first file-tier resolution for a given node binary, run the introspect once to derive the table for that exact node, cached on binary path + mtime, mirroring `discover_node`'s existing mtime cache.
- **Pro:** always correct, with no staleness reasoning and no version gate — the table is authoritative for the node in hand. **Con:** one extra `node` spawn per new node binary, ~54 ms once, amortized by the cache.

They compose: (A)'s baked table is the fast path, and (B) is what (A)'s fallback does, optionally memoized. Given the churn in §8 — a frozen core and a slow niche tail — (A) is the right default, correct for ~100% of real command lines with zero spawns, with the version-gated fallback closing the only correctness gap. Adopt (B)'s mtime-keyed memoization for the fallback if the introspect ever fires often enough to matter.

## Appendix: reproduction

```sh
# Per-version authoritative dump (option name → OptionType, + aliases):
node --expose-internals dump-harness.js
# Run across majors via PATH/nvm; diff the value-accepting sets.
```

Ground-truth source: `node/src/node_options.{h,cc}` and `node_options-inl.h`, specifically the `Parse` value-consuming branch.

## Changelog

- 2026-06-28 — Initial write-up.
