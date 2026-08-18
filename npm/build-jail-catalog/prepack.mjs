// Copy the canonical catalog in at pack time.
//
// ⛔ THE CATALOG IS NOT COMMITTED HERE, DELIBERATELY. A second copy beside the canonical
// `crates/nub-sandbox/data/build-jail-catalog-v2.json` would drift the moment a grant is corrected, and it
// would drift SILENTLY: the compiled catalog and the published one would disagree while both looked fresh.
// Packing from the canonical file means the published artifact is the one the binary was built against.
//
// ⛔ REFUSES TO PACK AN UNSTAMPED CATALOG. The reader accepts an update only when its
// `provenance.generatedAt` is strictly newer than the compiled stamp, so a catalog packed without one can
// never be installed by anybody — it would publish successfully and then be refused on every machine.
import { copyFileSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const canonical = join(here, "..", "..", "crates", "nub-sandbox", "data", "build-jail-catalog-v2.json");
const text = readFileSync(canonical, "utf8");
const doc = JSON.parse(text);

const stamp = doc?.provenance?.generatedAt;
if (typeof stamp !== "string" || !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/.test(stamp)) {
  console.error(
    `refusing to pack: ${canonical} has no provenance.generatedAt in the fixed-width form the reader ` +
      `requires (got ${JSON.stringify(stamp)}). Without it no installation can ever be shown to be newer.`,
  );
  process.exit(1);
}

copyFileSync(canonical, join(here, "build-jail-catalog-v2.json"));
// The package version tracks the catalog's own date rather than nub's release version, because the point
// of publishing separately is that a grant fix does NOT wait for a nub release. Kept in step with the
// stamp so `npm view` and the reader's banner agree about which measurement this is.
const pkgPath = join(here, "package.json");
const pkg = JSON.parse(readFileSync(pkgPath, "utf8"));
const [y, m, d] = stamp.slice(0, 10).split("-").map(Number);
const want = `${y}.${m}.${d}`;
if (pkg.version !== want) {
  pkg.version = want;
  writeFileSync(pkgPath, `${JSON.stringify(pkg, null, 2)}\n`);
  console.error(`prepack: version set to ${want} from provenance.generatedAt`);
}
console.error(`prepack: packing the catalog generated ${stamp}`);
