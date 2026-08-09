// @nubjs/types — TypeScript 6+ entry point.
//
// TypeScript 6 ships the official Temporal declarations. Reference that focused
// library even when a consumer targets ES2024, then layer Nub's additive runtime
// augmentations from common.d.ts on top. Keeping this file a global script is
// intentional; see common.d.ts for the data-import wildcard invariant.
/// <reference lib="esnext.temporal" />
/// <reference lib="es2025.iterator" />
/// <reference path="./common.d.ts" />
