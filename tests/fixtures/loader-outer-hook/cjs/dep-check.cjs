// `dep/sub` resolves to the dependency's `sub.ts` only through the `.ts` key a
// user transpiler put in `require.extensions` — plain Node plus that handler
// finds it, and nub must not lose it in its own dependency-resolution retry.
console.log('dep-ok', require('dep/sub').answer);
