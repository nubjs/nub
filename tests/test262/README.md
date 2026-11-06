# Language polyfill conformance

`npm run test:test262` runs the Node 27 language polyfills against the official [Test262](https://github.com/tc39/test262) corpus. It fetches the exact commit in `config.json` and uses the official `test262-harness@10.0.0` for harness includes, strict/default scenarios, async completion, and failure reporting. The runner refuses broad roots, wildcard paths, unrelated APIs, duplicate selections, and execution counts above its focused budget. It never runs the complete Test262 suite.

The selected files cover `Math.sumPrecise`, `Intl.Locale.prototype.variants` and the constructor's `variants` option, `Iterator.zip`/`zipKeyed`, iterator `join`/`includes`/`take`/`drop`/`chunks`/`windows`, `Promise.try`, and async iterator disposal. `Intl.Locale` is wrapped to apply the option, so the selection also includes the constructor tests that wrapper could break; the rest of `test/intl402/Locale` depends on the host's ICU and fails on older Node versions with or without Nub. Updating the pin requires reviewing the changed upstream tests and adjusting the expected counts intentionally.

The prelude removes only the methods under test, then evaluates Nub's actual polyfill source inside each test realm. It also installs into realms created by `$262.createRealm()`. This exercises fallbacks even when Node has native methods. It does not alter the Test262 assertions or includes.

```sh
npm run test:test262
# Reuse an existing checkout at the exact pinned commit:
node tests/test262/run.mjs --checkout /path/to/test262
# Select a different Node binary:
node tests/test262/run.mjs --node /path/to/node
# Run one of four shards with two harness workers:
node tests/test262/run.mjs --shard 1/4 --workers 2
# Inspect the validated shard without starting the harness:
node tests/test262/run.mjs --shard 1/4 --list
# Compare the same tests against native Node 27 without polyfills, skipping `polyfillOnly`:
node tests/test262/run.mjs --node /path/to/node27 --native
```

The runner defaults to the available CPU count capped at four workers; `--workers` accepts 1 through 32. Shards partition sorted, tracked file paths by position, so their selections are deterministic, disjoint, and cover the same files together. Each file runs both default and strict scenarios. Invalid or empty shards fail before the harness starts. `--list` prints the revision, selected paths, worker count, and expected scenario counts as JSON.

The pinned sparse checkout is reused without fetching on a cache hit. New checkouts fetch without unrelated file contents and become visible only when complete, allowing concurrent local shards to share the cache. The pinned harness package also uses npm's cache.

## Known failures

`knownFailures` in `config.json` lists the tests that fail on a given Node major, each with its reason. A listed test that passes fails the run, as does any unlisted failure.

| Node | Tests | Cause |
| --- | --- | --- |
| 18, 20 | `zip` and `zipKeyed` `result-is-iterator` | Harness order: the intrinsic lookup runs before the prelude installs `Iterator` |

## CI

CI runs the forced-polyfill suite on Node 18.19, 24 and 26, one job per fallback path, each with two workers. A missing summary, unexpected scenario count, empty suite, modified tracked source, or wrong source revision also fails the run. Only tracked test files are selected. The script unit tests retain Node's parallel execution across test files. The standalone regression suite in `scripts/node27-polyfills.test.mts` additionally covers overflow/cancellation, installing without `Intl`, and the preservation of native methods.

The suite tests library APIs. Experimental import syntax, loader support, and engine-internal changes are outside this corpus.

## Unit coverage by shim family

`npm run test:scripts` discovers all of these files. Unit tests remove targeted native methods inside isolated subprocesses so each fallback actually executes. Existing native implementations are checked separately for preservation. These fast checks complement the bounded Test262 selection; they do not expand it.

| Shim family | Unit coverage |
| --- | --- |
| `Intl.Locale` variants, precise sums, `zip`/`zipKeyed`, iterator join/includes/take/drop, `Promise.try`, async iterator disposal | `scripts/node27-polyfills.test.mts`: validation, closure order, reentrancy, edge cases and native preservation |
| Remaining iterator constructor/helpers, concat, chunks/windows, sync disposal, `Symbol.metadata` | `scripts/polyfills.test.mts`: forced fallback without native helpers, lazy iteration, abstract construction, descriptors |
| Set algebra, Array.fromAsync, Map/WeakMap upsert | `scripts/polyfills.test.mts`: set-like inputs, async mapping/closure, subclass results, present-undefined keys and reentrancy |
| Promise.withResolvers, allKeyed/allSettledKeyed | `scripts/polyfills.test.mts`: subclass constructors, symbol keys, null-prototype results and rejection |
| JSON parse options | `scripts/polyfills.test.mts`: recursive freezing, prototype selection, revivers, validation and idempotence |
| Well-formed strings, Object/Map.groupBy, array/typed-array copies | `scripts/polyfills.test.mts`: surrogates, special keys, closure, nonmutation and indexing |
| ArrayBuffer transfer/detached, Atomics.pause, URL.parse | `scripts/polyfills.test.mts`: real detachment, resize policy, argument validation and coercion errors |
| Uint8Array base64/hex | `scripts/polyfills.test.mts`: alternate alphabet, partial writes and malformed input |
| DisposableStack, AsyncDisposableStack, SuppressedError | `scripts/polyfills.test.mts`: ownership moves, LIFO disposal, sync fallback, repeated disposal and error chaining |
| reportError, localStorage neutralization, File/Blob, MessageEvent ports | `scripts/polyfills.test.mts`: scheduled errors, throwing getters, builtin identity, version-specific freezing and lazy-global preservation |
| RegExp.escape, Error.isError | `scripts/polyfills.test.mts`: escaped literals, invalid inputs, subclasses and tag spoofing; cross-realm Error recognition remains the documented fallback limitation |
| URLPattern and Float16 package adapters | `scripts/polyfills.test.mts`: supplied-package identity, argument forwarding and independent native DataView preservation |
| Lazy Temporal/URLPattern/Float16 loaders and lazy-property helper | `scripts/runtime-shims.test.mts`: package-backed behavior, deferred loading, overrides, retries and native preservation |
| navigator identity and Web Locks | `scripts/runtime-shims.test.mts`: floor/modern behavior, FIFO ordering, abort/steal/ifAvailable, query and validation |
| Worker and worker-side globals | `scripts/runtime-shims.test.mts`: real message/transfer/termination, errors, blob URLs, name/close and native preservation |
| Floor-builtin facade | `scripts/runtime-shims.test.mts`: exports and absence of global leaks |
| util debounce/throttle | Existing `scripts/util-rate.test.mjs`: scheduling, cancellation, errors and module routing |
| Test262 selection safety | `scripts/test262-selection.test.mts`: exact pins, explicit implemented-feature allowlist, bounded workers and deterministic complete shard coverage; rejects whole-suite requests |

The package-adapter units test Nub's wiring, not every operation in third-party implementations. Existing Rust integration tests and worker WPT coverage remain in place for end-to-end runtime behavior.

## Captured runtime operations

`runtime/primordials.cjs` takes one frozen, null-prototype snapshot per module instance, following [Node's primordial references](https://github.com/nodejs/node/blob/main/lib/internal/per_context/primordials.js). It captures static functions and constructor references and converts prototype functions into receiver-first calls. It does not freeze application globals or eagerly read lazy web globals. The polyfill module loads it before installing fallbacks. The Test262 prelude evaluates the snapshot separately inside each realm, so constructors and errors belong to that realm.

`scripts/primordials.test.mts` poisons selected globals and prototype methods after installation in isolated VM realms. It checks precise sums, locales, joint iterators, Set algebra, map upsert/grouping, array copies, byte conversions and callback invocation. Spec-required reads of user iterator methods, set-like arguments, options, promise constructors and callbacks remain observable. This is protection for adopted internal operations, not a sandbox: existing host internals, third-party packages and remaining protocol-driven operations can still observe modified prototypes. Capturing after an earlier preload has changed a builtin cannot recover the original engine reference.

The npm runtime file list includes the snapshot, the embedded runtime archive includes its source tree, and compiled bundles reach it through the polyfill module's static CommonJS dependency.
