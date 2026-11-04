// @nubjs/types — TypeScript 6+ entry point.
//
// Every `reference lib` below names a library TypeScript already ships for a
// feature Nub GUARANTEES across its whole Node floor — native where the running
// Node has it, polyfilled below (feature_matrix.rs gives the band per feature).
// Pulling the focused libraries in HERE is what lets a consumer keep
// `lib: ["es2024"]` — the target `nub init` scaffolds — and still type that
// surface, rather than raising `lib` to `esnext` and picking up features Nub does
// not provide at all. Nub then layers its additive runtime augmentations from
// common.d.ts on top. Keeping this file a global script is intentional; see
// common.d.ts for the data-import wildcard invariant.
//
// A library reference is only safe where the name resolves in EVERY TypeScript the
// entry point serves: an unknown one is a hard TS2726 that fails the consumer's
// whole build. TypeScript keeps a promoted library's old name working as an alias
// (7.0 ships no lib.esnext.array.d.ts yet still resolves `esnext.array`), so the
// risk is a name that does not exist YET, not one that has moved. Anything a
// supported TypeScript lacks is hand-declared in common.d.ts instead — which is
// also why nothing here references lib.es2025.regexp, lib.esnext.collection,
// lib.esnext.error or lib.es2025.promise: common.d.ts already carries their whole
// contents. `es2025.collection` is a different library from `esnext.collection` —
// it holds the Set methods, which common.d.ts does not declare.
/// <reference lib="esnext.temporal" />
/// <reference lib="es2025.iterator" />
/// <reference lib="es2025.collection" />
/// <reference lib="esnext.array" />
/// <reference path="./common.d.ts" />
