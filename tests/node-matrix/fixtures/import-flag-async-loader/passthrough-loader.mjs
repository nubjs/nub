// Minimal pass-through async ESM loader — the shape tsx's registered loader takes. Its mere
// presence (async resolve/load, no sync surface) is the whole trigger; it does no work.
export async function resolve(specifier, context, nextResolve) {
  return nextResolve(specifier, context);
}
