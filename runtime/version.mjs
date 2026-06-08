// Single source of truth for the nub version that salts the transpile-cache key.
//
// `make version` rewrites the NUB_VERSION line below in lockstep with every npm
// package + Cargo.toml, and `make version-check` fails the release build if it
// drifts. Keeping it in its own side-effect-free module lets every transpile
// surface — runtime/transform-core.mjs (the shared core), runtime/preload.mjs
// (fast path), and the compat-tier loader worker — import the SAME value, so the
// fast and compat tiers produce byte-identical cache entries under one key. (It
// previously lived as a literal inside preload.mjs, which `make version` patched,
// while the worker carried a hand-maintained "…-compat" copy that `make version`
// never touched — a latent staleness bug this module closes.)
export const NUB_VERSION = "0.0.21";
