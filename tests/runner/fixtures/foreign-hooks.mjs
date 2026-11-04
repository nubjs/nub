import { readFileSync } from 'node:fs';

export async function load(url, context, nextLoad) {
  const result = await nextLoad(url, context);
  if (url.endsWith('/foreign-target.cjs')) {
    return { format: 'commonjs', source: readFileSync(new URL(url)), shortCircuit: true };
  }
  return result;
}
