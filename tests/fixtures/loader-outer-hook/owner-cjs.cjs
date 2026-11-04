// tsx's `--require`d preflight installs its `require.extensions['.ts']` before
// any `--import` runs — so on the compat tier it lands BEFORE nub's preload, and
// nub must leave it in charge rather than replace it.
const { readFileSync } = require('node:fs');
const { transformSource } = require('./owner-core.cjs');

require('node:module')._extensions['.ts'] = (mod, filename) => {
  mod._compile(transformSource(readFileSync(filename, 'utf8')), filename);
};
