// A user hook that labels a `.ts` file with a bare `commonjs` format but brings
// no load hook of its own: it still relies on nub as the transformer, so the
// bare format alone must not make nub hand Node the raw TypeScript.
require('node:module').registerHooks({
  resolve(specifier, context, nextResolve) {
    const resolved = nextResolve(specifier, context);
    return resolved.url.endsWith('/typed.ts') ? { ...resolved, format: 'commonjs' } : resolved;
  },
});
