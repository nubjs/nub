// A program that never writes the static form must run with V8's default flags. The
// dynamic form in dynamic-form.mjs then fails with bare Node's catchable SyntaxError,
// instead of the V8 fatal abort the flag turns it into.
try {
  await import("./dynamic-form.mjs");
  console.log("import-defer:dynamic-ran");
} catch (err) {
  console.log(`import-defer:dynamic-error=${err.name}`);
}
