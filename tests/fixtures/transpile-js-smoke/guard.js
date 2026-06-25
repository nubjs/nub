// PERMANENT REGRESSION GUARD — see the `js_transpilation_smoke_guard` test.
// A `v`-flag (ES2024 unicode-sets) RegExp literal is a hard PARSE error on the
// compat-tier floor (Node 18.19's V8 predates the `v` flag) — raw, it kills the
// whole module before any line runs. oxc lowers `/…/v` to a `new RegExp(…)`
// constructor at nub's es2022 target, so a transpiled module PARSES and the marker
// below executes — even on 18.19. The regex sits in an UNCALLED function so the
// constructor never runs (V8 on 18.19 would reject the `v` flag at construction);
// the guard is that the file PARSES + the marker prints, which only happens if nub
// transpiled this `.js`. If `.js` ever falls out of TRANSPILE_EXTS, the marker
// vanishes on the floor and this test fails. DO NOT DELETE.
function neverCalled() {
  return /[\p{Letter}]/v;
}
console.log("SMOKE-JS:ok");
