// `dep/plain` resolves to the dependency's `plain.cjs` only through a user's
// `.cjs` key in `require.extensions` — plain Node has none of its own.
console.log('dep-cjs-ok', require('dep/plain').answer);
