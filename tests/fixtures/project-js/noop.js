// A no-op project `.js` with NOTHING oxc lowers at es2022 — single quotes, no
// trailing semicolons, a trailing comma, blank lines, an object literal that oxc
// would reflow. The skip-gate must return this VERBATIM (byte-for-byte): the test
// reads `f.toString()` to prove nub never ran it through codegen (which would
// normalize quotes/semicolons/whitespace and append a sourcemap footer).
function f() {
  const x = 'single'
  const obj = { a: 1, b: 2, }
  return x + obj.a
}

console.log('NOOP:' + JSON.stringify(f.toString()))
