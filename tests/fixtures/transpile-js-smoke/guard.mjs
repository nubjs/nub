// PERMANENT REGRESSION GUARD — see the `js_transpilation_smoke_guard` test.
// `using` (ES2026 explicit resource management) is a hard SyntaxError on every
// supported Node's V8 (it is not shipped natively below the host line), so this
// `.mjs` ONLY runs if nub transpiled it. An ESM entry resolves the disposal helper
// on EVERY tier including the 18.19 floor (the CJS-entry helper limitation does not
// apply to ESM), so this is the floor-proof "a floor-breaking `.js`/`.mjs`/`.cjs`
// executes" assertion. If `.mjs` ever falls out of TRANSPILE_EXTS, this throws.
let out = [];
{
  using a = { [Symbol.dispose]() { out.push("a"); } };
  using b = { [Symbol.dispose]() { out.push("b"); } };
  out.push("body");
}
console.log("SMOKE-MJS:" + out.join(","));
