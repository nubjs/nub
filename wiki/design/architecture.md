# Architecture

Nub is a Rust CLI that augments the user's installed Node. It ships no runtime, patches no Node source, and embeds no `libnode`.

Everything Nub adds reaches the process through a mechanism Node already publishes:

| Surface | Carries |
| --- | --- |
| Preload injection | One entry file that registers everything below |
| Module hooks | TypeScript, JSX, path aliases, extensionless imports, data formats |
| Flag injection | Experimental features the installed Node has but keeps gated |
| Environment | Files read before the spawn, prepended to the child |
| Native addon | The transpiler, the TypeScript resolver, the data parsers |
| Path shim | A `node` that resolves back to Nub, so augmentation survives a subprocess |

> [!NOTE]
> The test that decides whether a feature is in scope: would a user on plain Node, plus the corresponding `module.register()` call, preload, or addon, get the same result? If not, the feature needs a different mechanism or it is dropped.

The choice of per-file hooks over a bundler pass is in [[research/augmentation-layers]].

## Feature support across Node versions

Nub supports Node 18.19 and above. Across that range a feature may be native, gated behind a flag, or absent — so making it work means a different action per version.

All of it lives in one table, 47 features deep. Each carries sorted, non-overlapping version bands, and each band names exactly one mitigation:

| Mitigation | What Nub does |
| --- | --- |
| Native | Nothing. The version already ships it. |
| Unflag | Injects the experimental flag, across the exact range where it exists and is still required |
| Polyfill | Installs a JavaScript polyfill, guarded by a `typeof` feature detect |
| Storage file | Passes a workspace-keyed `--localstorage-file` path, for Web Storage only |
| Unflag on argv | Injects a V8 flag Node accepts only on the command line, never through `NODE_OPTIONS` |
| Runtime V8 flag | Turns a V8 flag on from inside the process, the first time a module that uses its syntax is loaded |

Twelve distinct flags are injected this way, covering `node:sqlite`, EventSource, WebSocket, Web Storage, and the vm, wasm, addon and text-import module kinds. A thirteenth, `--js-defer-import-eval` for `import defer`, never rides the command line at all. Node refuses it in `NODE_OPTIONS` by name, and a V8 flag that is non-default at startup makes Node reject its embedded code cache for every internal module compiled afterwards, which cost every program on Node 26.4+ several milliseconds while the flag rode argv. So the preload turns it on with `v8.setFlagsFromString` the first time it loads a module whose source uses the syntax, which V8 honors because it reads that flag only in the parser, and a program that never uses the syntax runs with V8's default flags. The polyfilled set is web and TC39 globals: Temporal, URLPattern, Worker, `navigator`, Float16Array, the disposable stack types, and the iterator, promise and collection helpers.

Below a feature's floor no band matches and Nub does nothing — the feature is unavailable rather than half-present.

Banding is exact because it has to be. Injecting an experimental flag on a version that does not have it is a hard startup abort, not a warning. Several rows carry two disjoint bands where a backport reached one release line and not another; `node:sqlite` is the clearest case, having been unflagged, re-flagged, and unflagged again. ShadowRealm is never injected at all, because the flag crashes embedded Node through a snapshot hash mismatch. That hazard generalizes, and it is why no version-gated flag rides `NODE_OPTIONS`. That string is inherited by every process below, including an embedded Node booting from a V8 snapshot, and the set Nub builds is matched to the version of the Node it resolved — so a descendant running an older Node meets a flag it cannot parse and aborts at startup. Every injected flag travels on argv instead, reaching the process Nub spawns and nothing beneath it; a child `node` gets its own copy by re-entering Nub through the PATH shim. What stays in `NODE_OPTIONS` is the preload, which every Node accepts, and flags whose floor sits at or below Nub's own support floor.

The flag-injection logic in [[crates/nub-core/src/node/flags.rs#compute_inject_flags]] reads the table in [[crates/nub-core/src/node/feature_matrix.rs#FEATURES]] rather than keeping its own copy, so a version-gated claim traces to a row. A band that runs to infinity would eventually inject a flag the running Node has dropped, so both unflag shapes are checked against the real binary before injection, by different probes. The ordinary set is filtered against the list Node reports as accepted in `NODE_OPTIONS` — a cheap read that stays a valid existence check even though the flags themselves travel on argv. The argv-only and runtime sets cannot use that list, since a flag Node refuses there is absent from it by construction, so each is checked by spawning the binary with it once and caching the verdict. A flag the binary no longer takes is dropped rather than aborting it at startup. Surveys behind the bands: [[research/node-experimental-flag-lifecycle]], [[research/node-flag-arity]].

## Two tiers

The runtime exists in two shapes, chosen by the availability of the synchronous hooks API and carried as [[crates/nub-core/src/node/version.rs#SupportTier]]. The floor of 18.19 is set by what the extension mechanisms permit.

| | Fast tier | Compat tier |
| --- | --- | --- |
| Node | 22.15 and above, except 23.0 to 23.4 | 18.19 to 22.14, and 23.0 to 23.4 |
| Preload channel | `--require` | `--import` |
| Hooks | `module.registerHooks`, synchronous, in-thread | `module.register`, in a loader worker |
| Polyfills | Lazy getters | Eager import |

The 23.x exclusion is not a special case, it is the tier definition applied correctly. `module.registerHooks` is a semver-minor that reached the 23.x Current line at 23.5.0 and the 22.x LTS line only later, at 22.15.0, so 23.0 to 23.4 sorts above the 22.x floor while carrying no synchronous hooks API. A version comparison against 22.15 alone therefore claims a capability those releases do not have.

Using `--require` on the fast tier is a correctness mechanism, not an optimization. An `--import` preload forces eager ESM loader initialization, which routes even a CommonJS entry point through the async module job and breaks `executionAsyncId`, sync exception origin, `require.main.id` and `module.parent`. Coverage and composition behavior of the hooks API is measured in [[research/registerhooks-coverage-matrix]].

## TypeScript and resolution

Both ride the same hook pair, and both run in Rust behind a single call across the addon boundary.

The load hook handles type stripping, the non-erasable syntax other strippers refuse (enums, parameter properties, `namespace`, `import =`), JSX, legacy decorators with metadata emission, down-levelling of `using` and the RegExp `v` flag, and the YAML, TOML, JSON5 and JSONC loaders. Output is content-addressed on disk with the source map already inlined, so a cache hit does no work in JavaScript. Stage 3 decorators are not transformed; the runtime raises a diagnostic rather than emitting wrong code.

The resolve hook is additive only. It layers tsconfig path aliases, extensionless probing for TypeScript extensions, and Yarn Plug'n'Play reads on top of Node's own resolver, and returns nothing when it has no additive answer — at which point resolution falls straight through. There is no reimplementation of Node's resolution algorithm anywhere in Nub, which confines the risk to what Nub adds. That is validated by running Node's own resolution test subset twice, once in passthrough and once augmented, and asserting parity: [[research/resolution-conformance]].

When JavaScript syntax inspection identifies a required transform, the transform reuses the inspected bytes instead of reading the file again. Loader execution order remains unchanged on both tiers.

Background: [[research/tsgo-vs-oxc-for-transpile]], [[research/wasm-vs-napi-for-transpile]], [[research/emit-decorator-metadata]], [[research/tsconfig-paths]], [[research/ts-extension-precedence]].

## Composition

Real toolchains shell out. If augmentation stopped at the first process, TypeScript would work in an entry point and fail in everything it launched.

So Nub writes a private `node` into a temporary directory named with [[crates/nub-core/src/node/spawn.rs#PATH_SHIM_PREFIX]] and puts it first on `PATH` for the subtree it started. A child that spawns `node` lands back in Nub and gets the same treatment. The directory is per-invocation, owner-only, reclaimed on exit, and swept by a background reaper for runs that were killed. The argv contract that shim honors is in [[research/node-flag-hijack-compat]]; the general technique is surveyed in [[research/node-impersonation]].

The persistent shim installed by `nub node shim` is the opposite: it runs the resolved Node unaugmented. Version management is its job. A globally augmenting `node` would load environment files and inject globals into every Node process on the machine.

## Turning it off

Both `--node` and a truthy `NODE_COMPAT` disable runtime augmentation — no hooks, no preload, no injected flags, no path shim. They compose, and `NODE_COMPAT` is stamped tree-wide so every descendant inherits it.

Two details make the switch trustworthy. Compat mode does not merely skip augmentation; it restores a parent's augmented environment to its pre-Nub state. And version provisioning stays on, because running on stock Node and running on no particular Node are different requests.

## Environment files

Four files load in precedence order, with the real process environment always winning:

- `.env.<mode>.local`
- `.env.local`
- `.env.<mode>`
- `.env`

Loading happens in the CLI before the spawn rather than in a preload. Cross-runtime load order, the expansion subset, and the security case for the ordering are in [[research/env-file-loading]].

## How the code is laid out

Three Cargo workspaces, and the splits are structural rather than organizational.

| Workspace | Holds | Why it is separate |
| --- | --- | --- |
| Root | The CLI, Node discovery and spawn, version management, workspace handling | — |
| Native addon | The transpiler and parsers, as a cdylib | The panic strategy is profile-global, and a library loaded into the user's Node must unwind rather than abort |
| Package manager | The install engine, vendored | Its own workspace root |

The JavaScript runtime — preloads, hooks, polyfills, worker shims — is compressed into the binary at build time and inflated once into a per-user cache directory, verified against digests baked into the executable. Extraction publishes by rename, so a concurrent reader sees a complete directory or none.

The full transformer links into the addon only. The CLI binary carries a parser subset and never the transformer.
