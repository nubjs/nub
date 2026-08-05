# Node flag arity — the exhaustive value-accepting set, for file-tier subject resolution

## TL;DR / verdict

- **Scope:** this table is for the **Node/file tier only** — the one tier whose grammar is open + evolving + not ours. The nub-owned tiers (`run`/script, `dlx`/package, bin) keep their full clap parse; clap is correct there because we own those closed grammars. Only the file tier needs the arity-set + introspect fallback, because clap fails-closed on a future Node flag.
- **The whole problem reduces to one question per token:** *does this Node flag consume the next token as its value?* If yes, skip the flag **and** the next token; if no, skip just the flag; the first remaining non-`-` token is the file subject. That "consumes a value" set is the entire deliverable.
- **The value-accepting set is small and stable.** ~**72 canonical value-accepting node options** in current Node (v26), plus ~**9 value-accepting aliases** (`-e`, `-r`, `-C`, `--loader`, …). **44 of them are present byte-identically across Node 18 / 20 / 22 / 24 / 26** — the entire core (`--require`, `--import`, `--eval`, `--conditions`, `--experimental-loader`, `--max-http-header-size`, the `--cpu-prof-*`/`--heap-prof-*`/`--report-*`/`--test-*` value flags, …). Per-major churn is **+3 to +10 additions, 0–2 removals**, concentrated entirely in niche `--experimental-*` / `--test-*` / snapshot/SEA flags. The set is stable enough to hardcode.
- **Maintainer's hypothesis confirmed:** for node-native value options, anything that accepts `--flag=value` (inline) **also** accepts `--flag value` (space) — it is literally the same code path in the parser. There is **no** inline-only node-native value option.
- **V8 passthrough flags (`--max-old-space-size`, `--stack-size`, the ~972 `--v8-options`) are the one exception, and they help us:** node's parser does **not** consume their value token. `--max-old-space-size=4096` works (one token); `--max-old-space-size 4096` is *mislocated by node itself* (it treats `4096` as the first positional / subject and V8 errors). So for subject scanning, **every V8 flag, every boolean, every NoOp, and every unknown `--`-flag is zero-arity** — only node-native value options consume a separate token.
- **Forward-compat:** node *itself* rejects truly-unknown flags (`bad option: --foo`). The only "unknown-but-valid" flag is one added in a Node **newer than nubx's baked table**. The safe trigger is therefore: **target node newer than the baked baseline + an unrecognized `-`-token present → introspect fallback** (the already-benchmarked ~54 ms `node`-introspect slow path). This provably cannot silently mis-locate (walked below).
- **Recommendation:** bake a generated static const (the value set + the known zero-arity universe + baseline node version) and gate a per-invocation introspect fallback on `node.version > baseline`. Steady state = zero spawns; correctness backstop only fires when you run a node newer than your nub. The always-correct alternative — derive-once-per-node-binary and cache by mtime (mirrors `discover_node`) — trades one spawn per new binary for eliminating the staleness reasoning entirely. Both are minimal; pick per the nubx thread.

---

## 1. Scope: the file tier only (the tiers are asymmetric)

nubx resolves a raw argv to one of several tiers. The grammars are **not** symmetric, so there is no single universal pre-scan:

- **nub-owned tiers** (`run`/script, `dlx`/package, bin) — *we* define those grammars and know them completely. They use a full clap parse, which robustly signals both "this argv resolves to this tier" and "it does not." clap is the right tool there.
- **The Node/file tier** — Node's flags are open, evolving, and not ours. This is the **only** tier where clap would fail-closed on a future Node flag and break pass-through. It uses the arity table below + the introspect fallback.

This document enumerates the **Node** value-accepting flags for the file-tier subject scan. It does not govern the nub-owned tiers. The resolver architecture lives in the `nubx-flag-env-resolution-review` thread.

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

Three facts fall straight out of this, and they are the entire model:

1. **Only `kInteger`/`kUInteger`/`kString`/`kHostPort`/`kStringList` consume a value.** `kBoolean`, `kNoOp`, `kV8Option` never do (the `if` excludes them).
2. **Inline and space are the same path.** Any node-native value option accepts both `--flag=value` and `--flag value`. The `=` form is only *recognized* for double-dash names — short aliases like `-e` reach the value option after alias expansion and only support the space form (`-e=x` → `bad option`, verified).
3. **A space-form value flag consumes exactly the next token, and only if it is non-`-`.** If the next token starts with `-` (or argv is empty), it's a "requires an argument" error — node never skips a flag to grab a later value.

Two more from the top of the loop:

```cpp
while (!args.empty() && errors->empty()) {
  if (args.first().size() <= 1 || args.first()[0] != '-') break;   // (a)
  const std::string arg = args.pop_first();
  if (arg == "--") { ...; break; }                                 // (b)
  ...
  if (it == options_.end()) { v8_args->push_back(arg); continue; } // (c)
```

- **(a)** A token that is exactly `-` (size 1) or doesn't start with `-` **ends** option parsing → it is the first positional = the **subject**. So `-` is the stdin subject, not a flag.
- **(b)** `--` ends option parsing; the **next** token is the subject (even if it starts with `-`).
- **(c)** An **unknown** flag is pushed to `v8_args` and the loop **continues without consuming a value** — node defers to V8, which then either accepts it (a real V8 flag) or rejects it (`bad option`). So node treats unknown flags as zero-arity.

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

Harvested empirically from each Node's **own** authoritative metadata — `internalBinding('options').getCLIOptionsInfo()` (the same map `lib/internal/options.js` consumes; `type` is the `OptionType` enum), dumped across Node 18.20.4 / 20.19.0 / 22.15.0 / 24.14.0 / 26.2.0. Harness and raw per-version dumps are retained privately.

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

(The 44 ★ flags are the byte-stable core; full per-version lists in the sidecar dumps.)

### Value-accepting aliases — Node v26.2.0 (9)

These expand to a value option and so consume a token in space form. `-e`, `-r`, `-C` are stable across all 5 majors.

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

Aliases keyed with `=` (`--inspect=`, `--inspect-brk=`, …) expand **only** in inline form and never consume a separate token, so they don't affect the space-form scan. Aliases keyed `--x <arg>` (`--print <arg>`) only expand when a non-`-` token follows — that is the `-p`/`--print` eval chain in §5.

## 5. Tier-changing / tricky flags (enumerated, not guessed)

- **`-e` / `--eval`** — `kString`, consumes the next token as the script source. **No file subject follows** — the tier is "eval," not "file." (`node -e 'console.log(2+2)'` → `4`, verified.)
- **`-p` / `--print`** — subtle: `--print` itself is `kBoolean`. `node -p '1+1'` → `2` works via an **alias chain**: `-p` → `--print`; then the parser's alias loop matches `--print <arg>` (because a non-`-` token follows) → expands to `-pe` → `{--print, --eval}`, and `--eval` consumes the token. Net effect for the scanner: **`-p`/`--print` followed by a non-`-` token = eval mode, no file subject.** `--print` followed by a `-`-token or EOL stays a bare boolean (REPL print mode).
- **`--`** — ends option parsing; the **next** token is the subject (even if it starts with `-`). Verified.
- **`-`** (single dash) — **the subject**, meaning stdin. Size-1 breaks the option loop. `echo 'code' | node -` runs stdin. Not a flag, not a path.
- **Preload value flags** — `--require`/`-r` (StringList), `--import` (StringList), `--experimental-loader`/`--loader` (StringList), `--conditions`/`-C` (StringList). All consume one value each; **repeatable** (each occurrence consumes its own value — repetition does not change the scan, verified `node -r a -r b`). They do **not** change the tier — a file subject still follows.
- **Directory/file value flags** — `--cpu-prof-dir`, `--heap-prof-dir`, `--diagnostic-dir`, `--redirect-warnings`, `--report-dir`/`--report-filename`, `--title`, `--snapshot-blob`, `--icu-data-dir`, `--env-file` — all plain `kString`/`kStringList`, consume one value, no tier change. (`--title` is `kString`, **not** a no-arg flag — it consumes.)
- **V8 value flags** — `--max-old-space-size`, `--stack-size`, `--max-semi-space-size`, … — `kV8Option`; see §6, they consume **nothing** from node's view.

## 6. V8 passthrough flags — node consumes no value (this simplifies forward-compat)

`kV8Option` flags are pushed verbatim to `v8_args` and the loop **continues without consuming a value**. Consequence, verified on v26 and v20:

```
$ node --max-old-space-size=4096 s.js          # inline: one token → s.js runs
SUBJECT_RAN []
$ node --max-old-space-size 4096 s.js          # space form: node does NOT consume 4096
Error: Value for flag --max-old-space-size of type size_t is out of bounds ...
```

In the space form node treats `--max-old-space-size` as a valueless V8 flag, then `4096` (non-`-`) **breaks the option loop and becomes the first positional / subject** — and V8, handed `--max-old-space-size` with no value, errors. This is real Node behavior on every major. So:

- **For subject scanning, all ~972 V8 flags are zero-arity** (treat like booleans). If a user writes the broken space form, nubx mislocating `4096` exactly mirrors what node does — pure pass-through parity, which is correct.
- nubx does **not** need to enumerate the 972 V8 flags by name to scan correctly — any flag not in the §4 value set is scanned as zero-arity, and that is what node does too (V8 flags + booleans + unknowns all behave identically: zero token consumption).

## 7. Forward-compat: the unknown-flag fallback, proven safe

The risk the maintainer raised: `nubx --new-flag value subject.js` where `--new-flag` is a value flag added in a Node **newer** than nubx's baked table. A stale table treats `--new-flag` as zero-arity → picks `value` as the subject, but real (newer) node consumes `value` and the subject is `subject.js`. **Mislocation.**

Walk it rigorously against node's actual behavior:

1. **Truly-unknown flags are not a hazard.** Node *itself* rejects a flag it doesn't know: `node --totally-bogus s.js` → `bad option: --totally-bogus` (verified). So a flag unknown to *both* nubx and the target node is moot — node errors regardless of where nubx thought the subject was.
2. **The only hazard is "unknown to nubx, known to the target node"** — i.e. a value flag added between nubx's baked baseline and the target node version. This is detectable: nubx already knows the exact node it is about to invoke (`discover_node`), hence its version.
3. **Safe trigger:** *if the target node version is newer than the baked baseline **and** the argv contains a `-`-token nubx's table doesn't recognize, fall back to the `node`-introspect slow path* (already benchmarked ~54 ms). The introspect runs the real node, whose `getCLIOptionsInfo()`/own parse knows the new flag, so it cannot mislocate.
4. **Why this can't silently mis-locate:** the only path to a wrong subject is an unrecognized value flag consumed by the real node but not by nubx. Either (a) the flag is unknown to the target node too → node errors, no silent wrong run; or (b) it's new-in-a-newer-node → caught by the version-gate + unrecognized-token check → introspect, which is authoritative. There is no third case: a flag nubx *recognizes* it scans with the correct arity by construction.

The fallback fires only in the genuinely rare "running a node newer than your nub, with a flag your nub predates" case — and only when such an unrecognized token is actually present. Steady state: zero fallbacks.

## 8. Stability / churn across majors

Value-accepting **canonical option** counts and deltas (excludes aliases), measured from the per-version dumps:

| Transition | Added | Removed | Notes |
|---|---|---|---|
| 18.20 → 20.19 | +10 | −1 | added `--env-file`, `--allow-fs-read/write`, `--disable-warning`, `--test-timeout`, SEA/snapshot; removed `--experimental-specifier-resolution` |
| 20.19 → 22.15 | +10 | −2 | added `--run`, `--stack-trace-limit`, `--localstorage-file`, the `--test-coverage-*` family; removed `--experimental-policy`, `--policy-integrity` |
| 22.15 → 24.14 | +6 | −2 | added `--experimental-config-file`, `--test-global-setup`, `--watch-kill-signal`; removed `--experimental-default-type`, `--experimental-test-isolation` (→ renamed `--test-isolation`) |
| 24.14 → 26.2 | +3 | 0 | added `--build-sea`, `--experimental-test-tag-filter`, `--test-random-seed` |

Totals: 48 → 57 → 65 → 69 → 72. **Per-major delta is +3 to +10, removals 0–2.** Every addition is a niche `--experimental-*` / `--test-*` / diagnostic / SEA-snapshot flag — none touch the flags that realistically appear on a `nubx`/file-run command line. **The 44-flag core (every preload, eval, conditions, profiling, report, tls, http flag) is byte-stable across 18→26.** Removals are exclusively experimental features graduating or being dropped. The "they don't change much" claim holds: the *load-bearing* set is effectively frozen; churn lives in the long tail.

## 9. Recommendation: bake vs derive-and-cache

Two viable shapes; both are minimal:

**(A) Bake a generated static const + version-gated introspect fallback — recommended primary.**
- Generate (at nub build time, from `getCLIOptionsInfo()` of the latest Node) a const carrying: the value-accepting set (§4), the known zero-arity universe (booleans + V8 + NoOp — names only, cheap), and the baseline Node version.
- Scan offline with zero spawns. Per §7, fall back to the introspect slow path only when `target_node.version > baseline` **and** an unrecognized `-`-token is present.
- **Pro:** zero-cost in the steady state, fully offline, matches the maintainer's "one list + a fallback" shape. **Con:** the const is regenerated each nub release; correctness for newer-than-nub nodes rides entirely on the fallback (which is sound).

**(B) Derive-once-per-node-binary and cache by mtime — robust alternative.**
- On first file-tier resolution for a given node binary, run the introspect once to derive the table for *that exact node*, cache it keyed on the binary path+mtime (mirrors `discover_node`'s existing mtime cache).
- **Pro:** always correct, no staleness reasoning, no version-gate — the table is authoritative for the node in hand. **Con:** one extra `node` spawn per new node binary (amortized by the cache; ~54 ms one-time).

They compose: (A)'s baked table is the fast path; (B) is literally what (A)'s fallback does, optionally memoized. Given the churn data (§8) — a frozen core and a slow niche tail — **(A) is the right default**: the baked const will be correct for ~100% of real command lines with zero spawns, and the version-gated fallback closes the only correctness gap. Adopt (B)'s mtime-keyed memoization for the fallback if the introspect ever fires often enough to matter (it won't, in practice).

This is a nubx-resolver architecture call — final shape is the `nubx-flag-env-resolution-review` thread's to decide; this doc supplies the data and the safety proof.

## Appendix: reproduction

```sh
# Per-version authoritative dump (option name → OptionType, + aliases):
node --expose-internals dump-harness.js
# Run across majors via PATH/nvm; diff the value-accepting sets.
```

Ground-truth source: `node/src/node_options.{h,cc}`, `node_options-inl.h` (the `Parse` value-consuming branch). Raw per-version dumps and harness retained privately: `node-flag-arity-table.findings/`.

## Changelog

- 2026-06-28 — Initial write-up.
