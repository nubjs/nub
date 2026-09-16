// Models tsx's hook pair on the fast tier: its resolve hook gives a `.ts` file a
// bare `commonjs` format, and its load hook then expects the RAW source back from
// `nextLoad` so it can run its own module-format transform. mixed.ts mixes
// `import` with `require`, which only that outer transform can turn into CJS —
// nub's syntax detection would call it ESM.
const { readFileSync } = require('node:fs');
const { fileURLToPath } = require('node:url');
const { ownsFile, assertRaw, transformSource } = require('./owner-core.cjs');

require('node:module').registerHooks({
  resolve(specifier, context, nextResolve) {
    const resolved = nextResolve(specifier, context);
    return ownsFile(resolved.url) ? { ...resolved, format: 'commonjs' } : resolved;
  },
  load(url, context, nextLoad) {
    const loaded = nextLoad(url, context);
    if (!ownsFile(url)) return loaded;
    assertRaw(loaded);
    return { format: 'commonjs', source: transformSource(readFileSync(fileURLToPath(url), 'utf8')), shortCircuit: true };
  },
});
