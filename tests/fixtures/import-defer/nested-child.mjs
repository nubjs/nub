// Run by nested-negation.mjs with `--no-js-defer-import-eval`. The static form must
// stay a SyntaxError here, whatever environment the parent handed down.
try {
  await import("./entry.ts");
  console.log("nested:deferred");
} catch (err) {
  console.log(`nested:error=${err.name}`);
}
