// A `v`-flag (unicode-sets, ES2024) RegExp literal in project `.js`. The raw
// literal is a PARSE error on the compat-tier floor (Node 18.19's V8 predates the
// `v` flag), which kills the whole module. oxc lowers `/…/v` to a `new RegExp(…)`
// constructor at nub's es2022 target, so the module parses and runs everywhere the
// V8 RegExp engine supports `v` (Node 20+). This is the trigger the design's
// original verdict missed; without it this file fails to load on the floor.
const re = /[\p{Letter}]/v;
console.log("vflag:" + re.test("A"));
