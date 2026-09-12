exports.ownsFile = (url) => typeof url === 'string' && url.endsWith('/mixed.ts');

// nub must have stepped aside: the format it hands back is the one the resolve
// hook assigned, and the source is either absent (Node's default load for CJS)
// or the untouched file — nub's transpile would have stripped the `: number`.
exports.assertRaw = (loaded) => {
  if (loaded.format !== 'commonjs') {
    throw new Error(`mixed.ts came back as ${loaded.format}; nub transpiled a file the outer hook owns`);
  }
  if (loaded.source != null && !String(loaded.source).includes(': number')) {
    throw new Error('mixed.ts came back transformed; the outer hook expected the raw source');
  }
};

// Stands in for esbuild, for this one file.
exports.transformSource = (raw) => raw
  .replace("import { readFileSync } from 'node:fs';", "const { readFileSync } = require('node:fs');")
  .replace(': number', '');
