// The compat-tier (18.19-22.14) half of the ESM resolution repair: a `module.register` loader.
//
// WHY IT CARRIES THE TOLERANT WALK ITSELF. `module.register` hooks run on a SEPARATE loader
// thread, and `--import` preloads do not run there — so `windows_realpath_shim.js`, which the
// main thread got from `NODE_OPTIONS`, has not patched `fs.realpathSync` in this realm. Without
// it every `fs.realpathSync` here is the jail's refused one, the walk falls back to its input,
// and the hook hands back the LINK path: the silent wrong-version answer the repair exists to
// prevent. `esm_resolve_repair_node_options` therefore substitutes the shim's own source into
// the placeholder below rather than shipping a second implementation of the tolerance rule.
//
// The substituted source brings its own `node:fs`, `node:path` and `fileURLToPath` imports and
// installs behind its own per-realm `globalThis` sentinel, so this module adds only the one
// binding it needs beyond them.
__NUB_REALPATH_SHIM_SOURCE__

import { pathToFileURL as nubPathToFileURL } from "node:url";

const nubRealCache = new Map();
const nubReal = (p) => {
  let v = nubRealCache.get(p);
  if (v === undefined) {
    try {
      v = fs.realpathSync(p);
    } catch {
      v = p;
    }
    nubRealCache.set(p, v);
  }
  return v;
};

export async function resolve(specifier, context, nextResolve) {
  const r = await nextResolve(specifier, context);
  if (!r || typeof r.url !== "string" || !r.url.startsWith("file:")) return r;
  const u = new URL(r.url);
  const { search, hash } = u;
  u.search = "";
  u.hash = "";
  const out = nubPathToFileURL(nubReal(fileURLToPath(u)));
  out.search = search;
  out.hash = hash;
  return { ...r, url: out.href };
}
