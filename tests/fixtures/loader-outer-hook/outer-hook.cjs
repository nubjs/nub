// Registered AFTER nub's preload (appended to NODE_OPTIONS), so it runs ABOVE
// nub's load hook and sees nub's raw result — the position tsx occupies under
// `nub run`. It models the branch tsx takes on that result: a CommonJS result is
// left alone only when it carries a `file:` responseURL, as Node's default load
// always provides; anything else is treated as ESM, which is how a CJS-syntax
// `.ts` file ended up failing with `require is not defined`.
require('node:module').registerHooks({
  load(url, context, nextLoad) {
    const loaded = nextLoad(url, context);
    if (!url.endsWith('/entry.ts')) return loaded;
    if (loaded.format === 'commonjs' && !String(loaded.responseURL).startsWith('file:')) {
      throw new Error(`load result for entry.ts carries no file: responseURL (got ${loaded.responseURL})`);
    }
    return loaded;
  },
});
