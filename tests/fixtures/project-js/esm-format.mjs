// `.mjs` is always ESM regardless of package `type`. Top-level `import.meta.url`
// only works under the module format, so a correct `.mjs` format branch is what
// makes this run. (No transformable syntax → served verbatim, but with format=module.)
const isModule = typeof import.meta.url === 'string'
console.log('MJS:' + isModule)
