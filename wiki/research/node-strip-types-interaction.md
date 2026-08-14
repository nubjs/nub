---
**Status:** v1, 2026-05-18.
**Scope:** What Node's built-in `--experimental-strip-types` (unflagged in 23.6, stable in 24.12+/25.2+) does when a Nub `module.registerHooks()` load hook returns transpiled source for a `.ts` URL. Answer based on reading Node main-branch source: `lib/internal/modules/esm/{load.js, get_format.js, translators.js}` and `lib/internal/modules/typescript.js`.
**Builds on:** [[research/augmentation-layers]] (why Nub is on the registerHooks layer), [[research/module-resolution]] (the resolve-hook half).
---

# Node's strip-types vs `module.registerHooks()` — interaction model

Short answer: **Node's strip-types is dispatched off the `format` string returned by the load hook chain. Nub is in full control of whether the built-in transpiler runs on any file it intercepts.**

A load hook that returns `format: 'module'` (or `'commonjs'`) instead of `'module-typescript'` / `'commonjs-typescript'` stops Node stripping types.

## The strip-types format dispatch

Node has, in current main:

```js
// lib/internal/modules/esm/get_format.js:16-23
const extensionFormatMap = {
  '__proto__': null,
  '.cjs': 'commonjs',
  '.js':  'module',
  '.json': 'json',
  '.mjs': 'module',
  '.wasm': 'wasm',
};

// :25-35
function initializeExtensionFormatMap() {
  if (getOptionValue('--experimental-addon-modules')) {
    extensionFormatMap['.node'] = 'addon';
  }
  if (getOptionValue('--strip-types')) {
    extensionFormatMap['.ts']  = 'module-typescript';
    extensionFormatMap['.mts'] = 'module-typescript';
    extensionFormatMap['.cts'] = 'commonjs-typescript';
  }
}
```

Strip-types adds three entries to a format map keyed by extension, consulted by `getFileProtocolModuleFormat` when the load hook hasn't supplied a format already.

The stripping itself happens in **translators**, not in `load.js`:

```js
// lib/internal/modules/esm/translators.js:677-682
translators.set('module-typescript', function(url, translateContext, parentURL) {
  const { source } = translateContext;
  assertBufferSource(source, true, 'load');
  debug(`Translating TypeScript ${url}`, translateContext);
  translateContext.source = stripTypeScriptModuleTypes(stringify(source), url);
  return FunctionPrototypeCall(translators.get('module'), this, url,
                               translateContext, parentURL);
});

// :668-674 — same shape for 'commonjs-typescript'
// :336-340 — 'require-commonjs-typescript' for require()'d .cts
```

The translator dispatch table is keyed by **format string**. A `format` value of `'module-typescript'` selects the `module-typescript` translator, which calls `stripTypeScriptModuleTypes(source, url)` and then chains into the plain-`'module'` translator; a `format` value of `'module'` selects the plain translator directly. **Type-stripping is gated entirely on the format string the load hook chain produced.**

## How load.js treats a hook-supplied format

The relevant lines from `defaultLoad`, the bottom of the hook chain when no further user hooks remain:

```js
// lib/internal/modules/esm/load.js:62-116
function defaultLoad(url, context = kEmptyObject) {
  let { importAttributes, format, source } = context;
  // ...
  if (urlInstance.protocol === 'node:') {
    source = null;
    format ??= 'builtin';
  } else if (format === 'addon') {
    source = null;
  } else if (format !== 'commonjs') {
    if (source == null) {
      ({ responseURL, source } = getSourceSync(urlInstance, context));
      context = { __proto__: context, source };
    }
    if (format == null) {
      // Now that we have the source for the module, run `defaultGetFormat` to detect its format.
      format = defaultGetFormat(urlInstance, context);
      // ...
    }
  }
  validateAttributes(url, format, importAttributes);
  return { __proto__: null, format, responseURL, source };
}
```

The key line is `if (format == null) { format = defaultGetFormat(...) }`. `defaultGetFormat` consults the extension map, and it only runs if the format the hook chain produced is still nullish at the bottom of the chain.

**Conclusion: a load hook that returns `format: 'module'` for a `.ts` URL sets the format Node uses.** `defaultGetFormat` is not called, the extension map is not consulted, the `module-typescript` translator is not selected, `stripTypeScriptModuleTypes` is never invoked, and the plain `'module'` translator runs the transpiled source directly.

## Does strip-types run before or after the hook?

After, in dispatch order — strip-types is in the *translator* stage, which runs once the entire load-hook chain has produced its final `{ format, source }`:

```
resolve hook chain → load hook chain → defaultLoad → translator (by format)
```

Nub's hook participates in the load chain, so by the time strip-types would be considered the format is already locked in. To have Node strip the file, return `format: 'module-typescript'` with `source` as the raw `.ts` source; to use Nub's own transpilation, return `format: 'module'` with `source` as the swc/oxc output.

## How does the format string get set in the first place?

Two paths:

1. **A `format` hint flows from the resolve hook.** Node's `defaultResolve` runs `defaultGetFormat` itself, sets `format` on the resolution context, and passes that value into the load hook chain as `context.format`. So by the time Nub's load hook is called, `context.format` may already be `'module-typescript'` — if Node knows about strip-types and saw a `.ts` extension. The hook is free to return a different format.
2. **The load hook chain returns a format.** Either set explicitly, or left nullish so `defaultLoad` infers it from the URL extension — which is when the strip-types entry in the format map matters.

The resolve hook can also pre-empt the issue by returning `format: 'module'`. That hint flows into the load context and skips the `defaultGetFormat` call there too, giving two redundant places to nail this:

```js
const hook = {
  resolve(specifier, context, nextResolve) {
    const native = nubResolve(specifier, context.parentURL);
    if (native) {
      return {
        url: native.url,
        format: 'module',          // ← lock format here
        shortCircuit: true,
      };
    }
    return nextResolve(specifier, context);
  },

  load(url, context, nextLoad) {
    if (isOurInterceptedURL(url)) {
      const transpiled = nubTranspile(url);
      return {
        format: 'module',          // ← and here, belt-and-suspenders
        source: transpiled.source,
        shortCircuit: true,
      };
    }
    return nextLoad(url, context);
  },
};
```

Both should agree. Either alone suffices.

## Can Nub bypass Node's strip-types?

Yes. Two conditions are sufficient, and either alone is sufficient for `.ts`/`.mts`/`.cts` files:

1. Return a non-typescript `format` (e.g. `'module'`) from either the resolve hook or the load hook. This takes the file out of the `module-typescript`/`commonjs-typescript` translator dispatch entirely.
2. `shortCircuit: true`, to stop a later hook in the chain — or a future Node default behavior — from re-engaging strip-types after the call has been made.

Node has no "force strip" path on a file the hook chain owns; the translator dispatch follows the `format` string.

**Caveat — `node:` and `addon` paths are special-cased earlier in `defaultLoad` (lines 84-89): `node:` URLs and `addon` formats short-circuit before the format-inference branch.** Neither applies to `.ts`/`.tsx` content.

## Does the URL extension still matter once format is returned?

No. The extension matters for `defaultGetFormat`'s extension-map lookup, and that lookup is only consulted when `format == null` at the end of the load chain.

Once `format: 'module'` is returned, Node treats the URL as ESM JavaScript regardless of whether it ends in `.ts`, `.tsx`, or something synthesized.

The corollary: **rewriting the URL to a `.js` extension is not needed to escape strip-types.** Some loader designs do that — rewrite `file:///foo.ts` to a synthetic `file:///foo.ts.js` — to avoid built-in TS handling. Keeping the original `.ts` URL preserves source-map fidelity and debugger UX, so stack traces show the user's actual file. Bun's transpile-on-import preserves the original URL for the same reason.

## Should Nub inject `--experimental-strip-types` itself?

In 2026 the flag is `--strip-types`, off only via `--no-strip-types`. Unflagged in 23.6, stable in 24.12 LTS / 25.2. Per [[research/module-resolution]], Nub's pinned Node floor is ≥ 24, so strip-types is on by default.

**Recommendation: leave strip-types on (Node's default). Don't inject any flag; don't try to disable.** Reasons:

1. **Strip-types only activates for files Nub doesn't claim.** The load hook claims every `.ts`/`.tsx` file in the project. Strip-types fires only outside that intercept, which in practice means files inside `node_modules` — which Node refuses to strip, per the `isUnderNodeModules` check in `lib/internal/modules/typescript.js` — or pathological cases like a `.ts` file Node sees before the hook registers.
2. **Belt-and-suspenders against hook-registration races.** If anything bypasses the hook for a `.ts` file (Node loads something before the prelude finishes registering, an uninjected worker thread, a user clearing hooks mid-process via some future API), strip-types keeps the file running. Disabling it would turn "transpiled by Node's built-in instead of ours" into an `ERR_UNKNOWN_FILE_EXTENSION` crash.
3. **Cost is zero.** Strip-types does nothing if no `.ts` files reach the default loader, which is the steady state when the hook is active. The flag being on carries no compile-time or startup overhead.
4. **Compatibility with vanilla-Node mode.** The vanilla-Node-faithful entry point wants Node-default behavior, so the flag stays untouched there too. Same answer at both ends.
5. **`--no-strip-types` is a footgun.** Any startup win from skipping `initializeExtensionFormatMap`'s strip-types branch is measured in microseconds, against the loss of fallback safety and a state the user can't predict from a flag ("our hook on but Node's builtin off").

If an actual conflict turns up — Node's strip-types mutating state the Nub transpiler later reads — revisit. None has been identified.

## What about pre-process `register()` (async) hooks?

Async `module.register()` hooks have the same format dispatch logic; translators are universal, not specific to sync vs async hooks. Everything in this doc applies to either API.

Nub chose sync ([[research/augmentation-layers]]) for the unified `require`/`import` story and the in-realm dispatch; the format behavior is the same.

## Test surface

Worth committing to a smoke test in the prototype:

1. **A `.ts` file with non-erasable syntax** (an enum, say). Verify the hook handles it — swc strips it — and Node's strip-types does not run on it. If strip-types ran, it would throw `ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`, so a successful run proves the bypass.
2. **A `.ts` file with strip-types-incompatible syntax that swc handles** (e.g. `import = require(...)`). Same logic — should succeed.
3. **A `.ts` file Node sees first** (e.g. a bug where the prelude import order is wrong). Verify the strip-types fallback works and the race gets noticed and fixed. This is the case that would have crashed under a forced `--no-strip-types`.
4. **A `.cts` file under CJS interop.** Verify the `require-commonjs-typescript` translator path doesn't fire — the hook's `format: 'commonjs'` return should win over the `.cts → commonjs-typescript` extension-map entry.

## Relevant PRs

The upstream changes that set the current behavior: two that shipped strip-types, one that fixed its interaction with module detection, one that implemented the sync hooks, and the loaders WG tracking issue.

- **[#56350](https://github.com/nodejs/node/pull/56350)** — unflagged `--experimental-strip-types` in 23.6 (Jan 2025). The PR that made TS-in-Node a default-on experience.
- **[#60600](https://github.com/nodejs/node/pull/60600)** — marked strip-types stable in 24.12 LTS / 25.2 (early 2026). Renamed the flag from `--experimental-strip-types` to `--strip-types`.
- **[#54164](https://github.com/nodejs/node/pull/54164)** — fixed the strip-types ↔ `--experimental-detect-module` interaction. Background for why `defaultGetFormat`'s `.ts` branch re-runs `detectModuleFormat` on the stripped source: it needs the post-strip JS to decide ESM-vs-CJS in typeless packages.
- **[#55698](https://github.com/nodejs/node/pull/55698)** — implemented `module.registerHooks()` (sync hooks). The API this entire doc is about.
- **[loaders#208](https://github.com/nodejs/loaders/issues/208)** — loaders WG tracking issue for strip-types ↔ loaders interaction. No surprises landed; the dispatch story is what `translators.js` shows.

## Bottom line

Return `format: 'module'` from the load hook for any `.ts`/`.tsx` file Nub transpiles — that is the whole mechanism for owning the file.

Leave `--strip-types` at Node's default as a safety net for files Nub doesn't claim, and keep the original `.ts` URL rather than hiding the extension.

## Sources

The Node main-branch source files every claim above was read from, with the line ranges that carry the format dispatch, plus Node's own TypeScript documentation.

- `lib/internal/modules/esm/load.js` ([download](https://raw.githubusercontent.com/nodejs/node/main/lib/internal/modules/esm/load.js)) lines 62-116, 136-171: `defaultLoad` / `defaultLoadSync`, the `format == null` short-circuit gate.
- `lib/internal/modules/esm/get_format.js` ([download](https://raw.githubusercontent.com/nodejs/node/main/lib/internal/modules/esm/get_format.js)) lines 16-35: `extensionFormatMap` + `initializeExtensionFormatMap`, the `--strip-types`-gated TS extension entries. Lines 164-237: `getFileProtocolModuleFormat`, the per-extension dispatch (including the `.ts` strip-then-detect branch).
- `lib/internal/modules/esm/translators.js` ([download](https://raw.githubusercontent.com/nodejs/node/main/lib/internal/modules/esm/translators.js)) lines 336-340, 668-682: the three `*-typescript` translators that call `stripTypeScriptModuleTypes`.
- `lib/internal/modules/typescript.js` ([download](https://raw.githubusercontent.com/nodejs/node/main/lib/internal/modules/typescript.js)): `stripTypeScriptModuleTypes` itself; the `node_modules` reject.
- Node TS docs: [nodejs.org/api/typescript.html](https://nodejs.org/api/typescript.html).

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
