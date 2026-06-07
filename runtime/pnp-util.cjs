// Shared Yarn PnP ESM-resolution helper, used by BOTH realms that resolve modules:
// the main-thread preload (preload-common.cjs, fast tier) and the compat-tier
// loader worker (preload-async-hooks.mjs). Both resolve PnP specifiers identically
// — through PnP's public, conditions-free `resolveRequest` — and both must hand
// Node an explicit module `format`, so the logic lives here once rather than being
// copy-pasted across two threads where it could drift.
//
// Why the explicit format: without it, Node <= 20.11 mis-detects a zip-stored `.js`
// file from a `"type":"module"` package as CommonJS, routes it through the CJS
// translator, and `require()`s the ESM source -> ERR_REQUIRE_ESM. Newer Node gets
// it right on its own; emitting the format makes PnP ESM deps work down to the
// 18.19 floor. (CJS resolution stays in preload-common.cjs's `_resolveFilename`
// branch — it returns a path, not a hook result, and is main-thread-only.)
const { readFileSync } = require("node:fs");
const { join, sep } = require("node:path");
const { fileURLToPath, pathToFileURL } = require("node:url");

// A directory issuer for pnpapi.resolveRequest: cwd with a trailing separator so
// PnP treats it as a directory. `path.sep` (not a literal "/") keeps it correct on
// Windows, where cwd uses backslashes and a mixed "C:\x/" can confuse resolution.
const cwdIssuer = () => process.cwd() + sep;

// Module format of a PnP-resolved file. `.mjs`/`.cjs` are unambiguous; a `.js` file
// inherits its package's `type` (read via PnP — `fs` is zip-patched in both realms).
// `null` lets Node decide (non-JS, or detection failed).
function pnpFormat(pnp, resolvedPath) {
  if (resolvedPath.endsWith(".mjs")) return "module";
  if (resolvedPath.endsWith(".cjs")) return "commonjs";
  if (!resolvedPath.endsWith(".js")) return null;
  try {
    const info = pnp.getPackageInformation(pnp.findPackageLocator(resolvedPath));
    const pj = JSON.parse(readFileSync(join(info.packageLocation, "package.json"), "utf8"));
    return pj.type === "module" ? "module" : "commonjs";
  } catch {
    return null;
  }
}

// Resolve an ESM `specifier` (from `parentURL`) through PnP and return a Node
// resolve-hook result `{ url, format?, shortCircuit }`, or `null` if PnP can't
// resolve it (then the caller delegates to Node's default resolver). Throwing is
// the caller's signal to fall through too — callers wrap in try/catch.
function pnpResolveEsm(pnp, specifier, parentURL) {
  const issuer = parentURL ? fileURLToPath(parentURL) : cwdIssuer();
  const resolved = pnp.resolveRequest(specifier, issuer);
  if (!resolved) return null;
  const format = pnpFormat(pnp, resolved);
  return { url: pathToFileURL(resolved).href, shortCircuit: true, ...(format && { format }) };
}

module.exports = { pnpResolveEsm, cwdIssuer };
