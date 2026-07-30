---
**Status:** v1, 2026-05-18.
**Scope:** What Node's built-in `--experimental-strip-types`
(unflagged in 23.6, stable in 24.12+/25.2+) does when a Nub
`module.registerHooks()` load hook returns transpiled source for a
`.ts` URL. Definitive answer based on reading Node main-branch
source: `lib/internal/modules/esm/{load.js, get_format.js,
translators.js}` and `lib/internal/modules/typescript.js`.
**Builds on:** [`augmentation-layers.md`](augmentation-layers.md)
(why we're on the registerHooks layer in the first place),
[`module-resolution.md`](module-resolution.md) (the resolve hook
half).
**Informs:** Hook-prelude design under `lib/internal/nub/`; the
`format` field our load hook returns.
---

# Node's strip-types vs `module.registerHooks()` — interaction model

Short answer: **Node's strip-types is dispatched off the `format` string returned by the load hook chain. If our load hook returns `format: 'module'` (or `'commonjs'`) instead of `'module-typescript'` / `'commonjs-typescript'`, Node will not strip types. We are completely in control of whether the built-in transpiler runs on any file we intercept.**

This is the load-bearing fact for Nub's loader design. The rest of this doc walks the code path that proves it.

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

So strip-types adds three entries to a format map keyed by extension. The map is consulted by `getFileProtocolModuleFormat` when the load hook hasn't supplied a format already.

The actual stripping happens in **translators**, not in `load.js`:

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

The translator dispatch table is keyed by **format string**. A `format` value of `'module-typescript'` selects the `module-typescript` translator, which calls `stripTypeScriptModuleTypes(source, url)` and then chains into the plain-`'module'` translator. A `format` value of `'module'` selects the plain translator directly. **Type-stripping is gated entirely on the format string the load hook chain produced.**

## What load.js actually does with hook-supplied format

The relevant lines from `defaultLoad` (the bottom of the hook chain — when no further user hooks remain):

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

The key line is `if (format == null) { format = defaultGetFormat(...) }`. `defaultGetFormat` is what consults the extension map, and it only runs if the format the hook chain produced is still nullish at the bottom of the chain.

**Conclusion: if our load hook returns `format: 'module'` for a `.ts` URL, that's the format Node uses.** `defaultGetFormat` is not called. The extension map is not consulted. The `module-typescript` translator is not selected. `stripTypeScriptModuleTypes` is never invoked. The plain `'module'` translator runs our transpiled source directly.

## Does strip-types run before or after our hook?

**After** in the sense of dispatch order — strip-types is in the *translator* stage, which runs after the entire load-hook chain has produced its final `{ format, source }`. The pipeline order is:

```
resolve hook chain → load hook chain → defaultLoad → translator (by format)
```

Our hook participates in the load chain. By the time strip-types *would* be considered (translator dispatch), our format is already locked in.

So in mechanical terms, strip-types is downstream of our hook. We get the first say. If we want the file stripped by Node, we return `format: 'module-typescript'` (and `source` as the raw `.ts` source). If we want our own transpilation used, we return `format: 'module'` (and `source` as the swc/oxc output).

## How does the format string get set in the first place?

Two paths:

1. **A `format` hint flows from the resolve hook.** Node's `defaultResolve` runs `defaultGetFormat` itself, sets `format` on the resolution context, and that value is passed into the load hook chain as `context.format`. So by the time our load hook is called, `context.format` may already be `'module-typescript'` (if Node knows about strip-types and saw a `.ts` extension). We don't have to respect that; we return whatever format we want.
2. **The load hook chain returns a format.** Either we set it explicitly, or we leave it nullish and let `defaultLoad` infer it from the URL extension (which is when the strip-types entry in the format map matters).

Our resolve hook can also pre-empt the issue by returning `format: 'module'` from resolve. That hint flows into the load context and skips the `defaultGetFormat` call there too. So we have two redundant places to nail this:

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

## Can we definitively bypass Node's strip-types?

Yes. Two conditions are sufficient (and either alone is sufficient for `.ts`/`.mts`/`.cts` files):

1. Return a non-typescript `format` (e.g. `'module'`) from either the resolve hook or the load hook.
2. `shortCircuit: true` to stop further hooks in the chain from downstream-overriding our format.

The first condition takes us out of the `module-typescript`/`commonjs-typescript` translator dispatch entirely. The second prevents another registered hook (or a future Node default behavior) from re-engaging strip-types after we've made the call.

There's no "force strip" path Node can take on a file the hook chain owns. The translator dispatch is honest about following the `format` string.

**Caveat — `node:` and `addon` paths are special-cased earlier in `defaultLoad` (lines 84-89): `node:` URLs and `addon` formats short-circuit before the format-inference branch.** Neither applies to `.ts`/`.tsx` content, so this doesn't change the answer for our use case.

## Does the URL extension still matter once we've returned format?

No. The extension matters for `defaultGetFormat`'s extension-map lookup, and that lookup is only consulted when `format == null` at the end of the load chain. Once we've returned `format: 'module'`, Node treats the URL as ESM JavaScript regardless of whether the URL ends in `.ts`, `.tsx`, or something we synthesized.

The corollary: **we don't need to rewrite the URL to a `.js` extension to escape strip-types.** Some loader designs do that ("rewrite `file:///foo.ts` to a synthetic `file:///foo.ts.js`") to avoid built-in TS handling. We don't have to. Keeping the original `.ts` URL preserves source-map fidelity and debugger UX (stack traces show the user's actual file). Bun's transpile-on-import preserves the original URL for the same reason.

## Should we inject `--experimental-strip-types` ourselves?

(In 2026 the flag is `--strip-types`, off only via `--no-strip-types`. Unflagged in 23.6, stable in 24.12 LTS / 25.2. Per [`module-resolution.md`](module-resolution.md), Nub's pinned Node floor is ≥ 24, so strip-types is on by default for us.)

The question is whether we want it **on** for files we don't intercept, or **off** to keep the runtime behavior consistent with our transpiler.

**Recommendation: leave strip-types on (Node's default). Don't inject any flag; don't try to disable.** Reasons:

1. **Strip-types only activates for files we don't claim.** Our load hook claims every `.ts`/`.tsx` file in the project — that's the whole point of having a hook. Strip-types would fire only on files outside our intercept, which in practice means files inside `node_modules` (which Node refuses to strip — see `lib/internal/modules/typescript.js` `isUnderNodeModules` check) or pathological cases like a `.ts` file Node sees before our hook registers. Both are edge cases.

2. **Belt-and-suspenders against hook-registration races.** If anything happens to bypass our hook for a `.ts` file (Node loads something *before* our prelude finishes registering, a worker thread we forgot to inject, a user explicitly clearing hooks mid-process via some future API), strip-types is a reasonable fallback that at least keeps the file running. Disabling strip-types in those edge cases would turn a "transpiled by Node's built-in instead of ours" outcome into an `ERR_UNKNOWN_FILE_EXTENSION` crash. The former is strictly better.

3. **Cost is zero.** Strip-types does nothing if no `.ts` files reach the default loader, which is the steady state when our hook is active. There's no compile-time or startup overhead from the flag being on.

4. **Compatibility with vanilla-Node mode.** `nub node` (the vanilla-Node-faithful entry point per PLAN.md) wants Node-default behavior. Don't strip the flag in that mode; let Node's built-in handle it. Same answer at both ends.

5. **`--no-strip-types` is a footgun we shouldn't fire.** Even if we *could* gain a marginal startup win by skipping `initializeExtensionFormatMap`'s strip-types branch, the cost is measured in microseconds. Not worth the loss of fallback safety or the complication of having two modes ("our hook on but Node's builtin off" is a state the user can't predict from a flag).

If we ever discover an actual conflict — e.g. Node's strip-types mutating state our transpiler later reads — we revisit. None has been identified.

## What about pre-process `register()` (async) hooks?

`module.register()` async hooks have the same format dispatch logic — translators are universal, not specific to sync vs async hooks. Anything in this doc applies to either API. We chose sync ([`augmentation-layers.md`](augmentation-layers.md)) for the unified `require`/`import` story and the in-realm dispatch; the format behavior is the same.

## Test surface

Worth committing to a smoke test in the prototype:

1. **A `.ts` file with non-erasable syntax** (an enum, say). Verify our hook handles it (swc strips it) and Node's strip-types *doesn't* run on it. If Node's strip-types ran, it would throw `ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`. A successful run proves we bypassed it.
2. **A `.ts` file with strip-types-incompatible syntax that swc handles** (e.g. `import = require(...)`). Same logic — should succeed.
3. **A `.ts` file Node sees first** (e.g. a bug where the prelude import order is wrong). Verify strip-types fallback works and we notice and fix the race. This is the "would have crashed if we forced `--no-strip-types`" case.
4. **A `.cts` file under CJS interop.** Verify the `require-commonjs-typescript` translator path doesn't fire — our hook's `format: 'commonjs'` return should win over the `.cts → commonjs-typescript` extension-map entry.

## Relevant PRs

- **[#56350](https://github.com/nodejs/node/pull/56350)** — unflagged `--experimental-strip-types` in 23.6 (Jan 2025). The PR that made TS-in-Node a default-on experience.
- **[#60600](https://github.com/nodejs/node/pull/60600)** — marked strip-types stable in 24.12 LTS / 25.2 (early 2026). Renamed the flag from `--experimental-strip-types` to `--strip-types`.
- **[#54164](https://github.com/nodejs/node/pull/54164)** — fixed the strip-types ↔ `--experimental-detect-module` interaction. Background context for why `defaultGetFormat`'s `.ts` branch re-runs `detectModuleFormat` on the stripped source — it needs the post-strip JS to decide ESM-vs-CJS in typeless packages.
- **[#55698](https://github.com/nodejs/node/pull/55698)** — implemented `module.registerHooks()` (sync hooks). The API this entire doc is about.
- **[loaders#208](https://github.com/nodejs/loaders/issues/208)** — loaders WG tracking issue for strip-types ↔ loaders interaction. No surprises landed; the dispatch story is what `translators.js` shows.

## Bottom line

**Return `format: 'module'` from our load hook for any `.ts`/`.tsx` file we transpile.** That's the entire mechanism for owning the file. Don't touch the `--strip-types` flag — leave Node's default on as a safety net for files we don't claim. Don't rewrite URLs to hide the `.ts` extension — the format string is what gates strip-types, not the extension. Don't worry about strip-types running on our transpiled output — the translator dispatch table makes that mechanically impossible when our format is non-typescript.

## Sources

- `lib/internal/modules/esm/load.js` ([download](https://raw.githubusercontent.com/nodejs/node/main/lib/internal/modules/esm/load.js)) lines 62-116, 136-171: `defaultLoad` / `defaultLoadSync`, the `format == null` short-circuit gate.
- `lib/internal/modules/esm/get_format.js` ([download](https://raw.githubusercontent.com/nodejs/node/main/lib/internal/modules/esm/get_format.js)) lines 16-35: `extensionFormatMap` + `initializeExtensionFormatMap`, the `--strip-types`-gated TS extension entries. Lines 164-237: `getFileProtocolModuleFormat`, the per-extension dispatch (including the `.ts` strip-then-detect branch).
- `lib/internal/modules/esm/translators.js` ([download](https://raw.githubusercontent.com/nodejs/node/main/lib/internal/modules/esm/translators.js)) lines 336-340, 668-682: the three `*-typescript` translators that call `stripTypeScriptModuleTypes`.
- `lib/internal/modules/typescript.js` ([download](https://raw.githubusercontent.com/nodejs/node/main/lib/internal/modules/typescript.js)): `stripTypeScriptModuleTypes` itself; the `node_modules` reject.
- Node TS docs: [nodejs.org/api/typescript.html](https://nodejs.org/api/typescript.html).

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
