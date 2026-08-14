// @nubjs/types — TypeScript 6+ entry point.
//
// Every `reference lib` below names a library TypeScript already ships for a
// feature Nub polyfills across its whole Node floor (feature_matrix.rs). Pulling
// the focused libraries in HERE is what lets a consumer keep `lib: ["es2024"]` —
// the target `nub init` scaffolds — and still type the polyfilled surface, rather
// than raising `lib` to `esnext` and picking up features Nub does not polyfill.
// The names are version-routed: TypeScript 6 moved the ratified ES2025 features
// out of `esnext.*`, so ts5.9/index.d.ts spells the same libraries differently.
// Nub then layers its additive runtime augmentations from common.d.ts on top.
// Keeping this file a global script is intentional; see common.d.ts for the
// data-import wildcard invariant.
//
// Two libraries are deliberately NOT referenced here. TypeScript 6's
// lib.es2025.regexp and lib.esnext.collection contain nothing but RegExp.escape
// and Map/WeakMap getOrInsert, which common.d.ts already declares for every
// version, so referencing them would add no coverage while tying this file to a
// floating `esnext.*` surface. `es2025.collection` below is a different library —
// it carries the Set methods, which common.d.ts does not declare.
/// <reference lib="esnext.temporal" />
/// <reference lib="es2025.iterator" />
/// <reference lib="es2025.collection" />
/// <reference lib="es2025.promise" />
/// <reference lib="esnext.array" />
/// <reference lib="esnext.error" />
/// <reference path="./common.d.ts" />
