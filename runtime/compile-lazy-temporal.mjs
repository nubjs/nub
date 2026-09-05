// The ESM entry of the Temporal polyfill, re-exported for the CommonJS getter in
// compile-lazy-temporal.cjs to `require`. The package's CommonJS build requires
// `jsbi`, a dual package with no `exports` map, and the compile bundle resolves
// that with `module` before `main` (see the resolver options in
// crates/nub-cli/src/compile/bundle.rs) — so the CommonJS build would receive
// jsbi's ESM namespace and fail at `JSBI.BigInt`. The ESM build imports jsbi's
// default export, and is the shape the preamble bundled eagerly before. A
// `require` of this module hands the getter its namespace, evaluated on that
// first call and never before.
export { Temporal, toTemporalInstant } from "@js-temporal/polyfill";
