// PERMANENT REGRESSION GUARD — see the `js_transpilation_smoke_guard` test.
// Same floor-proof shape as guard.js, for the `.cjs` extension: a `v`-flag RegExp
// literal (raw parse error on the 18.19 floor) in an UNCALLED function. The module
// only PARSES + prints the marker if nub transpiled this `.cjs`. Guards `.cjs`
// membership in TRANSPILE_EXTS (and the classic require shim's `.cjs` handling).
// DO NOT DELETE.
function neverCalled() {
  return /[\p{Letter}]/v;
}
console.log("SMOKE-CJS:ok");
