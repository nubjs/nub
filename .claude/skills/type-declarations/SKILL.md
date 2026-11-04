---
name: type-declarations
description: >-
  Add, audit, or update @nubjs/types declarations for Nub runtime APIs. Use
  whenever Nub adds or changes a user-visible global, built-in method, module
  loader type, or TC39 proposal; when TypeScript or @types/node gains an
  overlapping declaration; and before every release to reconcile runtime
  changes since the previous tag. Covers conflict-safe global augmentation,
  compiler-version routing, fixture matrices, and package verification.
metadata:
  internal: true
---

# Writing Nub type declarations

`@nubjs/types` describes user-visible runtime behavior that the consumer's selected TypeScript libraries and `@types/node` do not already describe. Runtime truth comes first: declarations follow shipped behavior; they never advertise an unimplemented convenience or preserve a stale shape for compatibility.

The package lives under `npm/nub-types/`:

- `common.d.ts` — declarations shared by every supported compiler.
- `index.d.ts` — the current-compiler entry point and focused standard-library references.
- `ts5.9/index.d.ts` — the legacy entry selected through `typesVersions`.
- `test/fixtures/` — compile-time compatibility and negative controls.

## Start with an ownership audit

1. Trace the runtime surface from its implementation and its row in `crates/nub-core/src/node/feature_matrix.rs`. Check `runtime/polyfills.cjs`, `runtime/preload-common.cjs`, loader modules, or other owning code rather than inferring a signature from a feature name.
2. Check the exact TypeScript libraries supported by the fixture matrix and current `@types/node`. ECMAScript built-ins normally belong to TypeScript's `lib.*.d.ts`; Node and web globals may belong to `@types/node`.
3. Consult primary declarations: TypeScript's `src/lib` or installed `lib.*.d.ts`, DefinitelyTyped's `types/node`, and Bun's types when looking for a proven step-aside pattern. A proposal README or specification establishes semantics, but the official TypeScript declaration is the compatibility target once one exists.
4. Add a declaration only for the remaining gap. If a focused TypeScript library already owns the surface, reference that library instead of copying it.

## Choose the conflict-safe pattern

### Merge members into existing built-ins

Augment the interface that owns an existing global:

```ts
interface PromiseConstructor {
  allKeyed<T extends object>(promises: T): Promise<{ -readonly [K in keyof T]: Awaited<T[K]> }>;
}
```

Use the same pattern for `IteratorObject`, `Math`, `Atomics`, `SymbolConstructor`, typed-array instances, and typed-array constructors. Never redeclare `var Promise`, `var Symbol`, or another built-in value just to add a method. Method members merge as overloads when a future standard library adds them.

Property members are stricter: their modifiers and type must match the official settled declaration exactly. `Symbol.metadata`, for example, must remain `readonly` and `unique symbol`.

### Step aside from an optional global owner

Some globals are absent from Node's normal type environment but collide when a consumer also selects `lib.dom`. Follow the conditional `globalThis` pattern used by Node and Bun types: detect the other library, adopt its constructor type, and reduce Nub's interface extension to `{}`. The current helpers are `__NubLibDomIsLoaded` and `__NubUseLibDomIfAvailable` in `common.d.ts`.

Do not solve a global collision with `skipLibCheck`, a broad `any`, or a second incompatible `declare var`. The fixtures compile with `skipLibCheck: false` specifically to expose these conflicts.

### Route declarations that cannot merge

Classes, type aliases, and some namespace members cannot safely coexist with a later official library. Route those declarations by compiler version through `typesVersions`:

- TypeScript 5.9 and earlier receive Nub's inlined Temporal declarations.
- TypeScript 6 and later reference the official `esnext.temporal` library.
- Both routes reference `common.d.ts` for declarations that merge safely.

Keep version routing as narrow as possible. Add a new route only after reproducing a real collision, and set its boundary to the first compiler that ships the official declaration. Do not fork the whole package for one incompatible surface.

### Preserve the global-script invariant

`common.d.ts` must remain a global script. Never add a top-level `import`, `export`, or `export {}`: doing so makes wildcard declarations such as `declare module "*.yaml"` module-local and silently removes Nub globals from consumers.

Use triple-slash `lib` and `path` references in versioned entry points. If a declaration needs an external type, prefer a qualified ambient reference that does not convert `common.d.ts` into a module.

## Match runtime and inference precisely

- Copy an official TypeScript signature byte-for-byte when Nub backfills an API already declared by a newer compiler.
- For a proposal without official TypeScript declarations, derive the narrowest useful signature from the proposal and runtime implementation. Preserve keys, readonly removal, `Awaited`, generic buffer types, and overload behavior where they are observable to callers.
- Document runtime facts the type system cannot express, such as null prototypes or enumerable-key filtering, without pretending the signature enforces them.
- Keep vendored declarations tied to the bundled implementation version. When the Temporal polyfill changes, reconcile the TypeScript 5.9 fallback against that exact package version.
- Do not duplicate surfaces already supplied by the selected TypeScript library or `@types/node`.

## Add the smallest comprehensive fixture coverage

Every changed public declaration needs a concrete use in `test/fixtures/positive`; assert useful inferred types rather than merely checking that a property exists.

When a declaration could later land upstream, also update `test/fixtures/future-stdlib/official-proposals.d.ts` with the expected official duplicate. That fixture must compile with both declaration sources active and `skipLibCheck: false`.

Preserve the rest of the matrix:

- TypeScript 5.9 exercises the legacy route.
- TypeScript 6 exercises the first official Temporal route.
- The current preview/compiler exercises forward compatibility.
- `stepaside-dom` and `stepaside-stub` verify global-owner coexistence.
- `negative-export` must fail only because YAML resolution and `reportError` disappear after `common.d.ts` is converted into a module.

Run the fixture package through Nub against a freshly packed tarball. Do not rely on the checked-in `file:..` dependency for release verification: a package-manager content store may reuse an older local-package snapshot, which can hide or invent routing failures. The tarball workflow also proves the artifact users receive:

```bash
tmp=$(mktemp -d)
cp -R npm/nub-types "$tmp/nub-types"
cd "$tmp/nub-types"
nub pack --json
TARBALL=$(find "$PWD" -maxdepth 1 -name '*.tgz' -print -quit)
python3 - "$PWD/test/package.json" "$(basename "$TARBALL")" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
data["devDependencies"]["@nubjs/types"] = f"file:../{sys.argv[2]}"
path.write_text(json.dumps(data, indent=2) + "\n")
PY
cd test
nub install
nub run test
```

If the repository's minimum-release-age policy rejects a deliberately current compiler pin, pass `--minimum-release-age=0` for this disposable verification install; do not weaken the repository policy.

## Verify the published package, not just source files

When adding an entry point or shared declaration, include it in `npm/nub-types/package.json`'s `files` list. Keep `types` and `typesVersions` aligned with the on-disk layout. Avoid a package `exports` map unless every supported module-resolution mode and `typesVersions` route has been proven: an exports condition can bypass compiler-version routing.

From `npm/nub-types/`, run `nub pack --dry-run --json` and inspect the file list. For every release, use the real-tarball fixture workflow above. A passing source-tree fixture does not prove the npm artifact contains the routed file or that compiler-version routing survives installation.

## Mandatory release audit

Before every release, set `PREV=$(git describe --tags --abbrev=0)` and inspect `$PREV..HEAD` for user-visible runtime changes. At minimum, review:

```bash
git diff --name-only "$PREV"..HEAD -- \
  runtime/ crates/nub-core/src/node/feature_matrix.rs npm/nub-types/ \
  wiki/runtime/ site/content/docs/
```

For every added or changed API, record one conclusion while working:

1. the selected TypeScript library or `@types/node` already declares the exact shipped shape; or
2. `@nubjs/types` has been updated, covered by the compiler/collision fixtures, and verified in the packed artifact.

This audit is a release blocker and happens before `make version`. An empty declaration diff is valid only after every runtime change has an explicit owner.

## Common failure modes

- Assuming a Node runtime release means `@types/node` owns an ECMAScript proposal.
- Declaring an entire constructor when an interface member merge is sufficient.
- Copying a future official class or type alias into `common.d.ts` instead of routing by compiler version.
- Testing only one TypeScript version or enabling `skipLibCheck`.
- Updating `index.d.ts` but omitting the new file from the package allowlist.
- Adding a top-level import/export to `common.d.ts` and losing wildcard data modules.
- Trusting a source-tree test without inspecting and testing the packed tarball.
