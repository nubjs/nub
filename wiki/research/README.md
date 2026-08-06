# Research

Write-ups behind Nub's design decisions: measured results, ecosystem surveys, and the investigations that settled a choice one way rather than another. Code comments and design docs link here instead of restating the reasoning.

Each document records what was asked, how it was measured, and what the answer was. Most carry a changelog at the bottom, one dated bullet per revision, with a `REVERSAL:` marker where a later finding overturned an earlier one. A document is corrected in place rather than rewritten, so a conclusion that has moved on says so.

Roadmap and per-command planning material is deliberately not here.

## Node runtime mechanics

| Document | What it establishes |
| --- | --- |
| [node-flag-arity](node-flag-arity.md) | The exhaustive set of value-accepting Node CLI options, and how much it churns per major |
| [node-flag-interactions](node-flag-interactions.md) | Per-flag verdicts on how Node's CLI surface interacts with an augmentation layer |
| [node-flag-hijack-compat](node-flag-hijack-compat.md) | The argv proxy contract for a binary that answers to the name `node` |
| [node-experimental-flag-lifecycle](node-experimental-flag-lifecycle.md) | Whether an unflagged experimental flag survives, and what happens when it does not |
| [experimental-flags-unflagging](experimental-flags-unflagging.md) | Survey of the experimental flag surface across Node 22, 24 and 25/26 |
| [node-version-floor](node-version-floor.md) | Which Node version floor the extension mechanisms actually permit |
| [node-version-discovery](node-version-discovery.md) | Pin-file resolution and where installed Node versions live |
| [registerhooks-coverage-matrix](registerhooks-coverage-matrix.md) | Empirical coverage of the module hooks API, and sync/async composition |
| [snapshot-env-reads](snapshot-env-reads.md) | Why environment reads are live rather than captured in a V8 snapshot |
| [iterator-helpers-engine-support](iterator-helpers-engine-support.md) | Engine support for the iterator helpers proposal |

## Module resolution

| Document | What it establishes |
| --- | --- |
| [resolution-conformance](resolution-conformance.md) | A run of Node's own resolution test subset against the augmented runtime |
| [native-resolver-prior-art](native-resolver-prior-art.md) | What of Node's resolver already exists in C++ upstream |
| [upstream-cpp-resolver-prs](upstream-cpp-resolver-prs.md) | Whether anyone has ported the resolution algorithm itself |
| [rust-resolution-feasibility](rust-resolution-feasibility.md) | How much of Node's resolution can run in Rust before V8 starts |
| [ts-extension-precedence](ts-extension-precedence.md) | Extension precedence for extensionless imports across runtimes |
| [exports-map-ts-swap](exports-map-ts-swap.md) | The TypeScript-for-JavaScript swap inside an exports map |
| [tsconfig-paths](tsconfig-paths.md) | Path-alias resolution at runtime |
| [commonjs-handling](commonjs-handling.md) | Why detection beat flipping the default module format |
| [import-maps-node-resistance](import-maps-node-resistance.md) | Why Node has not shipped import maps |
| [import-maps-cross-runtime](import-maps-cross-runtime.md) | Import-map support across the non-browser runtimes |
| [module-resolution](module-resolution.md) | Extensionless ESM in TypeScript, and how close a hook layer can get to Bun |

## TypeScript and transpilation

| Document | What it establishes |
| --- | --- |
| [tsgo-vs-oxc-for-transpile](tsgo-vs-oxc-for-transpile.md) | The transpiler choice, measured |
| [wasm-vs-napi-for-transpile](wasm-vs-napi-for-transpile.md) | WebAssembly against a native addon for the transpile path |
| [node-swc-vs-oxc-choice](node-swc-vs-oxc-choice.md) | Why Node picked SWC for type stripping |
| [node-strip-types-interaction](node-strip-types-interaction.md) | How Node's own type stripping interacts with a load hook |
| [emit-decorator-metadata](emit-decorator-metadata.md) | What decorator metadata emission requires, and who depends on it |
| [bun-transpile-cache](bun-transpile-cache.md) | Bun's on-disk transpile cache, and whether the same shape holds |
| [tsx-architecture](tsx-architecture.md) | How tsx is built, and which parts are worth reusing |
| [augmentation-layers](augmentation-layers.md) | Bundler against loader hooks as the augmentation layer |

## Startup and performance

| Document | What it establishes |
| --- | --- |
| [cold-start](cold-start.md) | Where Node spends its first 12–20 ms |
| [script-runner-cold-start](script-runner-cold-start.md) | Script-runner startup across the package managers |
| [snapshot-architecture](snapshot-architecture.md) | Whether a V8 startup snapshot can carry a preload |
| [rust-from-js](rust-from-js.md) | Per-call overhead of the Rust-to-Node binding options |
| [napi-addon-structure](napi-addon-structure.md) | One addon or several |
| [clobber-perf-comparison](clobber-perf-comparison.md) | Native against userland for each replacement candidate |

## Build jail and sandboxing

Investigations behind the confinement applied to dependency lifecycle scripts during an install. The per-OS design ledgers live in [`../design/`](../design); these are the research documents that fed them.

### Prior art and mechanism

| Document | What it establishes |
| --- | --- |
| [sandbox-prior-art](sandbox-prior-art.md) | Whether a standard cross-platform sandboxing library exists, and why not |
| [sandbox-os-enforceability](sandbox-os-enforceability.md) | The capability-by-platform matrix: what each OS can actually enforce unprivileged |
| [sandbox-linux-deny-mechanisms](sandbox-linux-deny-mechanisms.md) | Linux in-place filesystem denial, mechanism by mechanism, and which suits which case |
| [sandbox-linux-userns-backend](sandbox-linux-userns-backend.md) | An optional namespace backend, and what it would close that Landlock cannot |
| [sandbox-linux-confinement-audit](sandbox-linux-confinement-audit.md) | Landlock and seccomp as actually applied |
| [sandbox-policy-provenance-patterns](sandbox-policy-provenance-patterns.md) | How other systems stop a confined process rewriting its own policy |
| [sandbox-carve-grant-set](sandbox-carve-grant-set.md) | Resolving allow and deny globs to a minimal grant set, and the crate survey behind the hand-roll |

### Network

| Document | What it establishes |
| --- | --- |
| [sandbox-private-range-egress](sandbox-private-range-egress.md) | What other sandboxes do about private-range egress by default |
| [sandbox-net-config-surfaces](sandbox-net-config-surfaces.md) | What a user can actually write to configure network policy |
| [sandbox-windows-net-parity](sandbox-windows-net-parity.md) | Why there is no unprivileged path to per-host egress on Windows |
| [sandbox-u5-mitm-security-audit](sandbox-u5-mitm-security-audit.md) | Independent security re-audit of the credential-brokering tier |
| [windows-sandbox-acl-prior-art](windows-sandbox-acl-prior-art.md) | How production sandboxes decide AppContainer reachability, and which APIs they avoid |
| [geteffectiverightsfromacl-invalid-acl](geteffectiverightsfromacl-invalid-acl.md) | Two ACE orderings that make a Win32 effective-rights query fail on legal DACLs |

### Filesystem and environment

| Document | What it establishes |
| --- | --- |
| [sandbox-macos-version-matrix](sandbox-macos-version-matrix.md) | Whether the environment-read closure holds across macOS versions |
| [sandbox-move-rename-bypass](sandbox-move-rename-bypass.md) | The per-OS verdict on relocating a secret out of a denied path |
| [sandbox-glob-deny-fidelity](sandbox-glob-deny-fidelity.md) | Fidelity of the glob-to-matcher translation on the deny path |
| [sandbox-os-essentials-env](sandbox-os-essentials-env.md) | Which environment variables the OS itself needs on a strip-all floor |
| [ci-env-var-for-lifecycle-scripts](ci-env-var-for-lifecycle-scripts.md) | Whether the runner should set `CI` for install scripts, and whether the corpus harness should |

### Executable axis

| Document | What it establishes |
| --- | --- |
| [sandbox-exec-allowlist](sandbox-exec-allowlist.md) | Whether an executable allowlist is a viable confinement axis |
| [sandbox-exec-disk-needs](sandbox-exec-disk-needs.md) | Which common tools fail when only their binary and libraries are readable |

### Grammar, structure and validation

| Document | What it establishes |
| --- | --- |
| [sandbox-cross-platform-grammar-audit](sandbox-cross-platform-grammar-audit.md) | Whether one policy grammar can mean the same thing on three operating systems |
| [sandbox-crate-structure](sandbox-crate-structure.md) | Structuring the engine so it is reusable outside Nub |
| [sandbox-pentest-macos](sandbox-pentest-macos.md) | An adversarial agent given free rein inside the macOS jail, and what it could not do |
| [build-jail-virgin-world](build-jail-virgin-world.md) | Running an install in a pristine OS and copying the result back |

## Ecosystem surveys

| Document | What it establishes |
| --- | --- |
| [monkey-patch-prevalence](monkey-patch-prevalence.md) | The blast radius of ignoring writes to Node's CommonJS resolver internals |
| [resolver-patcher-relevance-2026](resolver-patcher-relevance-2026.md) | Which resolver patchers still matter |
| [userland-package-clobbering-audit](userland-package-clobbering-audit.md) | Which userland packages are safe to replace with a native equivalent |
| [legacy-polyfill-clobber-candidates](legacy-polyfill-clobber-candidates.md) | A download-weighted sweep for polyfills Node has since absorbed |
| [clobber-feature-detect-audit](clobber-feature-detect-audit.md) | Whether each candidate feature-detects, and what breaks if it does not |
| [clobber-technical-followup](clobber-technical-followup.md) | The open questions left by the clobbering audits |
| [polyfill-demand-audit](polyfill-demand-audit.md) | Actual download demand for each polyfill under consideration |
| [store-marker-hardcoding](store-marker-hardcoding.md) | Packages that find the project root by hardcoding a virtual-store directory name |
| [workspace-discovery-walk-up](workspace-discovery-walk-up.md) | How each package manager finds the workspace root, including the fallback that walks the whole filesystem |
| [npm-corpus-data-sources](npm-corpus-data-sources.md) | Which registry endpoints and public datasets can answer install-script presence and per-version popularity at scale |

## Package manager

| Document | What it establishes |
| --- | --- |
| [gvs-in-ci](gvs-in-ci.md) | Why the global virtual store stays disabled under CI |
| [force-materialization-scope](force-materialization-scope.md) | Which packages break under a symlinked store, and the shape of the fix |
| [force-materialize-list-audit](force-materialize-list-audit.md) | A pre-ship confidence pass over the shipped list |
| [pnpm-filter-grammar](pnpm-filter-grammar.md) | The pnpm filter grammar and its resolution algorithm |
| [npm-config-user-agent](npm-config-user-agent.md) | What the user-agent string carries, and the scaffolder gap it opens |
| [nub-field-write-vs-detection](nub-field-write-vs-detection.md) | Whether a version range in the manifest survives five package-manager detectors |
| [pnpm-specific-behavior](pnpm-specific-behavior.md) | The pnpm-branded config and publish behaviors, and which to mirror |

## Environment files

| Document | What it establishes |
| --- | --- |
| [env-file-loading](env-file-loading.md) | Load order, precedence and mode suffixes across the runtimes and frameworks |
| [env-expansion-and-test-skip](env-expansion-and-test-skip.md) | The expansion subset every implementation agrees on |
| [env-autoload-security](env-autoload-security.md) | A confirmed code-execution escalation through an auto-loaded, committed environment file |

## Runtime architecture

| Document | What it establishes |
| --- | --- |
| [node-embedding-vs-spawn](node-embedding-vs-spawn.md) | Why the user's installed Node is spawned rather than embedded |
| [forking-node](forking-node.md) | The trade-offs of forking Node, and why that direction was abandoned |
| [node-impersonation](node-impersonation.md) | How a shim can stand in front of every `node` invocation in a process tree |
| [prototype-pollution-hardening](prototype-pollution-hardening.md) | Prior art on freezing intrinsics, and how much of the ecosystem it breaks |

## Compatibility measurements

| Document | What it establishes |
| --- | --- |
| [nub-v0.5-augmentation-regressions](nub-v0.5-augmentation-regressions.md) | Root causes for each augmentation regression against stock Node |
| [node-test-suite-leverage](node-test-suite-leverage.md) | Using Node's own test suite as a compatibility oracle |
| [native-http-transport](native-http-transport.md) | Whether a native HTTP transport helps once real Fetch semantics are required |

## Web platform APIs

| Document | What it establishes |
| --- | --- |
| [wintertc-node-gap-rationale](wintertc-node-gap-rationale.md) | Why Node has not shipped the last of the minimum common API |
| [globalthis-eventtarget-stress-test](globalthis-eventtarget-stress-test.md) | That rejection tested against Bun's lived experience of shipping it |
| [node-worker-threads-design-history](node-worker-threads-design-history.md) | Why Node's worker deviates from the web `Worker` |
| [node-worker-threads-pain-points](node-worker-threads-pain-points.md) | What developers actually hit when using worker threads |

## Data

Raw measurement output lives under [`data/`](data), one directory per topic.
