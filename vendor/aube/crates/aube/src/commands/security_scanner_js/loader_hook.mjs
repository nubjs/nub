// Node module-loader hook installed by the bridge runner via
// `module.register()`. Intercepts the `'bun'` specifier and
// routes it to the aube `Bun` shim that the bridge wrote next
// to this hook in the same temp dir.
//
// The shim path is passed via `parentURL` data on `register()`
// — node calls `initialize({ shimPath })` once before any
// resolve/load is invoked.

let shimPath = null;
const BUN_VIRTUAL_URL = 'aube-virtual:bun-shim';

export function initialize(data) {
  shimPath = data?.shimPath ?? null;
}

export function resolve(specifier, context, nextResolve) {
  if (specifier === 'bun' && shimPath) {
    return { url: BUN_VIRTUAL_URL, shortCircuit: true };
  }
  return nextResolve(specifier, context);
}

export async function load(url, context, nextLoad) {
  if (url === BUN_VIRTUAL_URL && shimPath) {
    const { readFile } = await import('node:fs/promises');
    const source = await readFile(shimPath, 'utf8');
    return { format: 'module', source, shortCircuit: true };
  }
  return nextLoad(url, context);
}
