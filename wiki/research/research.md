# Research

Write-ups behind Nub's design decisions: measured results, ecosystem surveys, and the investigations that settled a choice one way rather than another. Code comments and design docs link here instead of restating the reasoning.

Each document records what was asked, how it was measured, and what the answer was. Most carry a changelog at the bottom — one dated bullet per revision, with a `REVERSAL:` marker where a later finding overturned an earlier one. Documents are corrected in place rather than rewritten, so a conclusion that has moved on says so.

Roadmap and per-command planning material is deliberately not here. This file is the corpus index, and `lat check` fails if a document is missing from it.

## Node runtime mechanics

What Node's own CLI surface, version gating and hook APIs permit an augmentation layer to do.

- [[node-flag-arity]] — the exhaustive set of value-accepting Node CLI options, and how much it churns per major
- [[node-flag-hijack-compat]] — the argv proxy contract for a binary that answers to the name `node`
- [[node-experimental-flag-lifecycle]] — whether an unflagged experimental flag survives, and what happens when it does not
- [[node-version-discovery]] — pin-file resolution and where installed Node versions live
- [[registerhooks-coverage-matrix]] — empirical coverage of the module hooks API, and sync/async composition
- [[snapshot-env-reads]] — why environment reads are live rather than captured in a V8 snapshot
- [[iterator-helpers-engine-support]] — engine support for the iterator helpers proposal

## Module resolution

How much of Node's resolver can be reimplemented or intercepted, and where the ecosystem's expectations sit.

- [[resolution-conformance]] — a run of Node's own resolution test subset against the augmented runtime
- [[upstream-cpp-resolver-prs]] — whether anyone has ported the resolution algorithm itself
- [[rust-resolution-feasibility]] — how much of Node's resolution can run in Rust before V8 starts
- [[ts-extension-precedence]] — extension precedence for extensionless imports across runtimes
- [[exports-map-ts-swap]] — the TypeScript-for-JavaScript swap inside an exports map
- [[tsconfig-paths]] — path-alias resolution at runtime
- [[commonjs-handling]] — why detection beat flipping the default module format
- [[import-maps-node-resistance]] — why Node has not shipped import maps
- [[import-maps-cross-runtime]] — import-map support across the non-browser runtimes
- [[module-resolution]] — extensionless ESM in TypeScript, and how close a hook layer can get to Bun

## TypeScript and transpilation

The transpiler choice and the syntax surface it has to carry, measured rather than argued.

- [[tsgo-vs-oxc-for-transpile]] — the transpiler choice, measured
- [[wasm-vs-napi-for-transpile]] — WebAssembly against a native addon for the transpile path
- [[node-swc-vs-oxc-choice]] — why Node picked SWC for type stripping
- [[node-strip-types-interaction]] — how Node's own type stripping interacts with a load hook
- [[emit-decorator-metadata]] — what decorator metadata emission requires, and who depends on it
- [[bun-transpile-cache]] — Bun's on-disk transpile cache, and whether the same shape holds
- [[augmentation-layers]] — bundler against loader hooks as the augmentation layer

## Startup and performance

Where the milliseconds go before user code runs, and what each binding or caching option costs.

- [[cold-start]] — where Node spends its first 12–20 ms
- [[snapshot-architecture]] — whether a V8 startup snapshot can carry a preload
- [[rust-from-js]] — per-call overhead of the Rust-to-Node binding options
- [[napi-addon-structure]] — one addon or several
- [[clobber-perf-comparison]] — native against userland for each package the transpiler replaces

## Ecosystem surveys

Download-weighted sweeps of what real packages do, which bound how much the runtime can safely change.

- [[monkey-patch-prevalence]] — the blast radius of ignoring writes to Node's CommonJS resolver internals
- [[resolver-patcher-relevance-2026]] — which resolver patchers still matter
- [[userland-package-clobbering-audit]] — which userland packages are safe to replace with a native equivalent
- [[clobber-feature-detect-audit]] — whether each candidate feature-detects, and what breaks if it does not
- [[clobber-technical-followup]] — follow-up measurements to the clobbering audits
- [[store-marker-hardcoding]] — packages that find the project root by hardcoding a virtual-store directory name
- [[workspace-discovery-walk-up]] — how each package manager finds the workspace root, including the fallback that walks the whole filesystem
- [[npm-corpus-data-sources]] — which registry endpoints and public datasets can answer install-script presence and per-version popularity at scale
- [[preload-ecosystem]] — who depends on the preload channels, and what breaks them
- [[opentelemetry]] — what attaching OpenTelemetry costs, and what a launcher can decide before the process starts

## Package manager

Install-time behavior: store layout, filter grammar, and which of pnpm's branded surfaces to mirror.

- [[gvs-in-ci]] — why the global virtual store stays disabled under CI
- [[pnpm-filter-grammar]] — the pnpm filter grammar and its resolution algorithm
- [[npm-config-user-agent]] — what the user-agent string carries, and the scaffolder gap it opens
- [[nub-field-write-vs-detection]] — whether a version range in the manifest survives five package-manager detectors

## Environment files

Load order, expansion and the security consequences of reading a committed environment file automatically.

- [[env-file-loading]] — load order, precedence and mode suffixes across the runtimes and frameworks
- [[env-expansion-and-test-skip]] — the expansion subset every implementation agrees on

## Runtime architecture

Why Nub spawns the user's Node rather than embedding or forking it, and how it gets in front of the process.

- [[node-impersonation]] — how a shim can stand in front of every `node` invocation in a process tree

## Compatibility measurements

Runs against stock Node and against Node's own suite, used as the oracle for what augmentation broke.

- [[node-test-suite-leverage]] — using Node's own test suite as a compatibility oracle
- [[native-http-transport]] — whether a native HTTP transport helps once real Fetch semantics are required

## Web platform APIs

Which parts of the web platform Node has declined to ship, and what shipping them costs in practice.

- [[wintertc-node-gap-rationale]] — why Node has not shipped the last of the minimum common API
- [[globalthis-eventtarget-stress-test]] — that rejection tested against Bun's lived experience of shipping it
- [[node-worker-threads-design-history]] — why Node's worker deviates from the web `Worker`
- [[node-worker-threads-pain-points]] — what developers actually hit when using worker threads

## Data

Raw measurement output lives outside this corpus, under [`benchmarks/data/`](../../benchmarks/data), one directory per topic. `lat check` rejects a non-markdown file inside the graph, so the JSON moved there when the corpus became a lat graph.
