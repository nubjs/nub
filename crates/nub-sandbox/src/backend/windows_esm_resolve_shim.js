// Windows build jail: give a confined Node a working ES MODULE resolver.
//
// WHY THE REALPATH SHIM NEXT DOOR IS NOT ENOUGH. `windows_realpath_shim.js` replaces
// `fs.realpathSync`, and CommonJS picks that up because `internal/modules/helpers.js`'s
// `toRealPath` reads it as a PROPERTY at call time. The ESM resolver does not: on Node
// 18.19-24.16, 25.x and 26.0 `internal/modules/esm/resolve.js` opens with
// `const { realpathSync } = require('fs')`, capturing the binding when the module is first
// required — which is before any `--import` preload exists. So the ESM side keeps calling the
// jail's broken realpath and every relative or bare `import` dies `EPERM`, loudly, at rc=1 with
// no output. Measured across 50 installed Node builds: the source form predicted the behaviour
// 50/50, no exceptions.
//
// WAITING FOR NODE DOES NOT COVER US. nodejs/node#62835 ("restore fs patchability in ESM
// loader") switches the capture to a late `const fs = require('fs')` property read, and is in
// 24.17+ and 26.1+ — but it was NOT backported to v22.x, which is nub's fast-tier floor, and
// v25.9/v26.0 are strictly worse still (the resolver is in the V8 startup snapshot, so not even
// `--require` reaches it). Upstream also calls the restoration temporary and steers embedders to
// `module.registerHooks()`, which is what this file does.
//
// THE REPAIR, and why it is two seams rather than one. `--preserve-symlinks` stops the ESM
// resolver calling realpath at all, which clears the EPERM; on its own it then binds the WRONG
// VERSION of a dependency under nub's `Isolated` linker, silently (see
// `preserve_symlinks_isolated_layout`). So the realpath has to be put back, through the tolerant
// walk, at every seam the flag disarmed:
//
//   1. `Module._resolveFilename` — CommonJS, and NOT the same function as the
//      `resolveForCJSWithHooks` a registered resolve hook sees. `require.resolve()` and
//      `createRequire()` reach only this one; a hook-only repair measured `dep 1.0.0` where the
//      correct answer was `2.0.0`. It also covers `.node` addons, whose path reaches
//      `process.dlopen` through this function and whose `__dirname`-relative `bindings` /
//      `node-gyp-build` lookups derive from it.
//   2. A resolve hook — ESM, including `import.meta.resolve`, which measured as going through
//      the hook chain on every version in the band, async loader included.
//
// The main entry point is the one seam neither covers, because `--import` preloads run inside
// `executeUserEntryPoint`, after `resolveMainPath`. `--preserve-symlinks-main` (stamped by
// `realpath_shim_node_options`) is what keeps that one alive.
//
// DELIVERY is `--import` with a `data:` URL, for the reason on `windows_realpath_shim.js`:
// `defaultResolve` short-circuits on that protocol before touching the filesystem, so the
// preload needs no grant and the confined package cannot tamper with it.
import Module, { register } from "node:module";
import fs from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";

// The compat-tier loader module, as a `data:` URL — or `null` on an interpreter with
// `module.registerHooks`, where no loader thread exists to carry. Substituted by
// `esm_resolve_repair_node_options`.
const LOADER_URL = __NUB_ESM_LOADER_URL_JSON__;

// One walk per path, because the resolver asks for the same directories on every specifier and
// the tolerant walk is a per-component `lstat` loop rather than one syscall. A failed walk caches
// its input so a refusal outside the granted roots is not re-attempted on every resolution.
const cache = new Map();
const real = (p) => {
  let v = cache.get(p);
  if (v === undefined) {
    try {
      v = fs.realpathSync(p);
    } catch {
      v = p;
    }
    cache.set(p, v);
  }
  return v;
};

const origResolveFilename = Module._resolveFilename;
Module._resolveFilename = function (request, parent, isMain, options) {
  const filename = origResolveFilename.call(this, request, parent, isMain, options);
  // `isMain` is excluded deliberately: the main path was already decided by `resolveMainPath`
  // under `--preserve-symlinks-main` before this module existed, so rewriting it here would
  // move `require.main.filename` away from the path Node actually loaded.
  return isMain || typeof filename !== "string" ? filename : real(filename);
};

// The URL's query and hash are preserved across the rewrite: `import('./x.js?v=2')` is a distinct
// module from `./x.js` to the ESM cache, and dropping the search would collapse the two.
const repairUrl = (r) => {
  if (!r || typeof r.url !== "string" || !r.url.startsWith("file:")) return r;
  const u = new URL(r.url);
  const { search, hash } = u;
  u.search = "";
  u.hash = "";
  const out = pathToFileURL(real(fileURLToPath(u)));
  out.search = search;
  out.hash = hash;
  return { ...r, url: out.href };
};

if (typeof Module.registerHooks === "function") {
  Module.registerHooks({
    resolve(specifier, context, nextResolve) {
      return repairUrl(nextResolve(specifier, context));
    },
  });
} else if (LOADER_URL) {
  register(LOADER_URL);
} else {
  // FAIL LOUD, and this branch is the whole reason the loader may be gated off at all. The
  // caller decides whether to embed the loader from a version probe; if that probe is wrong the
  // ESM side is left with `--preserve-symlinks` and no repair, which resolves a dependency under
  // its link path and answers with a DIFFERENT version, silently. Aborting the confined Node
  // instead turns a mis-gate into a failed install, which is the direction this whole repair
  // exists to keep failures pointing.
  throw new Error(
    "nub build jail: no ESM resolution repair available on this Node (module.registerHooks is " +
      "absent and no loader was supplied)",
  );
}
