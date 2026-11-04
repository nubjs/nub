// The compiled artifact's `globalThis.Temporal`: the polyfill evaluates on the
// FIRST ACCESS, never at start. The twin of the preload's
// `installTemporalLazyGlobal` (preload-common.cjs, A37), which resolves the
// package from the runtime directory at that moment; here the package is bundled
// into the artifact, and the `require` below is what keeps the bundle lazy.
//
// CommonJS on purpose. Rolldown resolves a static-specifier `require` and bundles
// the package behind its own wrapper, which runs when the call does — inside the
// getter — so the chunk stays one file and a program that never touches Temporal
// never evaluates it. The eager `import` this replaces cost 16–24 ms per start
// (measured on Node 26, which strips the region as native; a Node 22 or 24
// target, or an unknown `--smol` one, kept it). An ESM `import()` cannot do this
// synchronously, and an `import` evaluates at start by definition.
//
// `installTemporalValue`, not `installTemporalGlobal`: the latter reads
// `globalThis.Temporal` first, which inside the getter is this getter. And the
// package's ESM entry through compile-lazy-temporal.mjs, not its CommonJS one —
// that file says why.
"use strict";
const { installTemporalValue } = require("./preload-common.cjs");
const { defineLazy } = require("./compile-lazy.cjs");

function installCompiledTemporalLazyGlobal() {
  if (typeof globalThis.Temporal !== "undefined") return;
  defineLazy(
    globalThis,
    "Temporal",
    () => installTemporalValue(require("./compile-lazy-temporal.mjs")),
    false,
  );
}

module.exports = { installCompiledTemporalLazyGlobal };
