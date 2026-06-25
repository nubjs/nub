// `.cjs` is always CommonJS regardless of package `type`. `module.exports` and the
// CJS `require` only exist under the commonjs format, so a correct `.cjs` format
// branch is what makes this run. (No transformable syntax → served verbatim, with
// format=commonjs.)
const isCjs = typeof module.exports === 'object' && typeof require === 'function'
console.log('CJS:' + isCjs)
