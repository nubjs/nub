// Shared Yarn PnP ESM-resolution helper, used by BOTH realms that resolve ESM
// imports: the main-thread preload (preload-common.cjs, fast tier) and the
// compat-tier loader worker (preload-async-hooks.mjs). CJS resolution does NOT live
// here — it just strips the `conditions` option and delegates to PnP's own patched
// `_resolveFilename` (see installCjsRequireHooks). ESM is different: PnP does not
// patch the ESM loader, so `import` of a PnP dep must be resolved explicitly, AND it
// must pass the *import* conditions — otherwise a dual package (separate `import` /
// `require` exports) resolves to its CJS build and `import { x }` fails. So we go
// through `pnpapi.resolveRequest` (the only PnP resolver that accepts conditions).
const { readFileSync } = require("node:fs");
const { dirname, join, sep } = require("node:path");
const { fileURLToPath, pathToFileURL } = require("node:url");

// A directory issuer for resolveRequest: cwd with a trailing separator so PnP treats
// it as a directory. `path.sep` (not a literal "/") keeps it correct on Windows.
const cwdIssuer = () => process.cwd() + sep;

// Module format of a resolved file. `.mjs`/`.cjs` are unambiguous; a `.js` file
// inherits its nearest package's `type`, read by walking up to package.json via the
// zip-patched `fs` (`.pnp.cjs` patches `fs`, so reads inside `.zip` work in both
// realms). No pnpapi needed. Defaults to "commonjs" if detection fails.
function formatOf(resolvedPath) {
  if (resolvedPath.endsWith(".mjs")) return "module";
  if (resolvedPath.endsWith(".cjs")) return "commonjs";
  let dir = dirname(resolvedPath);
  for (let i = 0; i < 16; i++) {
    try {
      const pj = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
      return pj.type === "module" ? "module" : "commonjs";
    } catch {}
    const up = dirname(dir);
    if (up === dir) break;
    dir = up;
  }
  return "commonjs";
}

// Resolve a `specifier` through PnP for a hook `context`, applying the correct
// exports conditions so a dual package resolves to the right build. Returns a Node
// resolve-hook result `{ url, format, shortCircuit }`, or `null` if PnP can't resolve
// it (the caller then delegates). Throwing is also a fall-through signal.
//
// Conditions: trust `context.conditions` when Node populates it (Node 24+/26 for both
// import and require; Node 22.15 for import). But Node 22.15 hands the resolve hook
// an EMPTY `conditions` for a `require()` — so when it's empty, infer the side from
// `importAttributes` (`undefined` ⇒ a require, an object ⇒ an import) and apply the
// matching default. Without this a 22.15 `require()` of a dual package would wrongly
// get the `import` build.
function pnpResolveEsm(api, specifier, context) {
  const parentURL = context && context.parentURL;
  const issuer = parentURL ? fileURLToPath(parentURL) : cwdIssuer();
  let conds = context && context.conditions;
  if (!conds || !conds.length) {
    const isImport = !!context && context.importAttributes !== undefined;
    conds = isImport ? ["node", "import"] : ["node", "require"];
  }
  const resolved = api.resolveRequest(specifier, issuer, { conditions: new Set(conds) });
  if (!resolved) return null;
  return { url: pathToFileURL(resolved).href, format: formatOf(resolved), shortCircuit: true };
}

module.exports = { pnpResolveEsm, cwdIssuer };
