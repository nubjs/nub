// nubx under Yarn PnP — resolve a bin name to its script and RUN it, the way
// `yarn exec` does. nub runs this file as its (augmented) entry, having injected
// `--require .pnp.cjs`, so `findPnpApi` is set up and the zipfs `fs` patch is live.
//
// Invoked as:  nub <this> <binName> [args...]
//
// Two things matter:
//   1. PnP has no `node_modules/.bin`, so we find the bin by walking the top-level
//      package's dependencies and matching `package.json#bin` (what Yarn's own bin
//      registry does), via the public pnpapi.
//   2. We load the resolved script with `require()` — the CJS path, where PnP's `fs`
//      patch reads zip-stored packages. Running it as a node *entry* instead would,
//      on the compat tier (Node <22.15, where nub augments via an `--import` ESM
//      preload), route the entry through the ESM loader whose existence check
//      bypasses PnP's `fs` patch and throws ERR_MODULE_NOT_FOUND on the zip path.
//      `require()` (with a dynamic-import fallback for an ESM bin) sidesteps that and
//      keeps nub's augmentation active in-process — matching `yarn exec` on every
//      supported Node.
const path = require("node:path");
const fs = require("node:fs");
const { pathToFileURL } = require("node:url");
const { cwdIssuer } = require("./pnp-util.cjs");

const want = process.argv[2];
const rest = process.argv.slice(3);

// `require("pnpapi")` throws for an out-of-tree issuer (this file lives in nub's
// install dir); `findPnpApi` resolves by the queried path, so it works here.
const api = require("node:module").findPnpApi(cwdIssuer());
if (!api) {
  process.stderr.write("nubx: not a Yarn PnP project\n");
  process.exit(127);
}

// bin name -> relative script path. A string `bin` is named after the package
// (its unscoped tail); an object maps explicit names.
function binsOf(pkg) {
  const b = pkg.bin;
  if (!b) return {};
  if (typeof b === "string") return { [(pkg.name || "").split("/").pop()]: b };
  return b;
}

let script = null;
const top = api.getPackageInformation(api.topLevel);
for (const [name, reference] of top.packageDependencies) {
  if (reference == null) continue;
  const info = api.getPackageInformation(api.getLocator(name, reference));
  if (!info) continue;
  let pkg;
  try {
    pkg = JSON.parse(fs.readFileSync(path.join(info.packageLocation, "package.json"), "utf8"));
  } catch {
    continue;
  }
  const rel = binsOf(pkg)[want];
  if (rel) {
    script = path.join(info.packageLocation, rel);
    break;
  }
}

if (!script) {
  process.stderr.write(
    `nubx: '${want}' not found in Yarn PnP dependencies.\n` +
      `      add it (yarn add ${want}), or run it ad-hoc with: yarn dlx ${want}\n`,
  );
  process.exit(127);
}

// Run the bin as if it were the entry: present its own argv, then load it via the
// zip-safe CJS path. Fall back to dynamic import for an ESM bin.
process.argv = [process.argv[0], script, ...rest];
try {
  require(script);
} catch (e) {
  if (e && e.code === "ERR_REQUIRE_ESM") {
    import(pathToFileURL(script).href).catch((err) => {
      console.error(err);
      process.exit(1);
    });
  } else {
    throw e;
  }
}
