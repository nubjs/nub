// A crash in a compiled binary must name the line the author WROTE, not a column
// in a minified bundle. `nub <file>` resolves a stack through the transpile; the
// compiler has to reach the same answer from a map generated at build time, and
// the pipeline that builds those mappings is the same one the syntax target and
// the JSX settings run through — so it can regress from a change aimed elsewhere.
//
// Only file and line are compared. The absolute paths differ by construction (the
// artifact reports a path inside its extraction directory) and the columns differ
// by a few characters after minification, but the line is the fact a person needs.
//
// Source-map support is not available on every Node, and nub knows it: Node
// 26.0.0–26.7.x corrupts a no-message `assert.ok(false)` into a TypeError once
// maps are on (nodejs/node#63169, fixed by 26.8.1), so `--enable-source-maps` is
// deliberately WITHHELD across that band — `source_maps_safe` in
// `crates/nub-core/src/node/flags.rs`. Asserting a mapped frame there asserts an
// augmentation nub is choosing not to make, and this harness defaults to 26.5.0,
// squarely inside it. So the band is read off the process rather than guessed,
// and the row goes vacuous on a Node where nub declines to remap.
//
// It is a REAL vacuum, not a suppressed failure: the same branch makes the plain
// Node column differ on a version where the flag IS injected, which it did not
// before. Type-stripping preserves line numbers, so plain Node named `app.ts` on
// its own and the row proved nothing on the very versions nub augments.
function inner(): never {
  throw new Error("boom");
}
function outer(): never {
  return inner();
}
try {
  outer();
} catch (e) {
  const frame = (e as Error).stack?.split("\n")[1] ?? "";
  const at = frame.match(/([^/\\]+\.ts):(\d+):\d+/);
  if (!process.sourceMapsEnabled) console.log("ok:no-source-map-support");
  else console.log(at ? `ok:${at[1]}:${at[2]}` : `ok:unresolved:${frame.trim().slice(0, 40)}`);
}
