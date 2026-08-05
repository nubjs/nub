// A crash in a compiled binary must name the line the author WROTE, not a column
// in a minified bundle. `nub <file>` resolves a stack through the transpile; the
// compiler has to reach the same answer from a map generated at build time, and
// the pipeline that builds those mappings is the same one the syntax target and
// the JSX settings run through — so it can regress from a change aimed elsewhere.
//
// Only file and line are compared. The absolute paths differ by construction (the
// artifact reports a path inside its extraction directory) and the columns differ
// by a few characters after minification, but the line is the fact a person needs.
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
  console.log(at ? `ok:${at[1]}:${at[2]}` : `ok:unresolved:${frame.trim().slice(0, 40)}`);
}
