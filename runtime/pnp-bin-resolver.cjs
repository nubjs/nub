// Resolve a bin name under Yarn PnP and print its script path. Spawned by nub as:
//   node --require <.pnp.cjs> pnp-bin-resolver.cjs <bin> <abs .pnp.cjs path>
// PnP has no node_modules/.bin, so walk the top-level package's dependencies via
// pnpapi and match the requested name against each dep's package.json#bin. Prints
// the absolute script path on success; exits 127 if no dep provides that bin.
//
// `require("pnpapi")` only resolves for scripts INSIDE the user's PnP tree; this
// resolver lives in nub's install dir (outside the tree), so we require() the
// absolute `.pnp.cjs` path nub passes as argv[3] — that returns the pnpapi object
// directly (getPackageInformation/getLocator/topLevel).
const api = require(process.argv[3]);
const fs = require("node:fs");
const path = require("node:path");
const want = process.argv[2];

function binsOf(pkg) {
  const b = pkg.bin;
  if (!b) return {};
  if (typeof b === "string") return { [(pkg.name || "").split("/").pop()]: b };
  return b;
}

const top = api.getPackageInformation(api.topLevel);
for (const [name, reference] of top.packageDependencies) {
  if (reference == null) continue;
  const info = api.getPackageInformation(api.getLocator(name, reference));
  if (!info) continue;
  let pkg;
  try { pkg = JSON.parse(fs.readFileSync(path.join(info.packageLocation, "package.json"), "utf8")); }
  catch { continue; }
  const rel = binsOf(pkg)[want];
  if (rel) { process.stdout.write(path.join(info.packageLocation, rel)); process.exit(0); }
}
process.exit(127);
