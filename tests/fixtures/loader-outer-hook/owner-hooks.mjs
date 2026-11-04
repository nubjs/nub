// The compat-tier twin of owner-hook.cjs, registered through `module.register`
// the way tsx does where `module.registerHooks` is absent. Like tsx there, the
// load hook passes a CommonJS result through untouched and lets Node's CJS
// loader run the `require.extensions` handler owner-cjs.cjs installed.
import { createRequire } from 'node:module';

const { ownsFile, assertRaw } = createRequire(import.meta.url)('./owner-core.cjs');

export async function resolve(specifier, context, nextResolve) {
  const resolved = await nextResolve(specifier, context);
  return ownsFile(resolved.url) ? { ...resolved, format: 'commonjs' } : resolved;
}

export async function load(url, context, nextLoad) {
  const loaded = await nextLoad(url, context);
  if (ownsFile(url)) assertRaw(loaded);
  return loaded;
}
