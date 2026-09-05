require('node:module').registerHooks({
  load(url, context, nextLoad) {
    if (url.endsWith('/state.mjs')) throw new Error('early hook saw transformable source');
    return nextLoad(url, context);
  },
});
