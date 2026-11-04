// The smallest virtual-module loader: it serves `custom:` itself and defers
// everything else. `index.js` gives the served URL a plain-JS extension, which is
// what makes nub's extension-keyed load branches consider claiming it.
export async function resolve(specifier, context, nextResolve) {
  if (specifier.startsWith("custom:")) return { url: specifier, shortCircuit: true };
  return nextResolve(specifier, context);
}

export async function load(url, context, nextLoad) {
  if (url.startsWith("custom:")) {
    return { format: "module", source: 'export default "served-by-custom-loader";\n', shortCircuit: true };
  }
  return nextLoad(url, context);
}
