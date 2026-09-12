// Checks an installed tree against the project's own package-lock.json. Starting from the
// importers — the root and every workspace member — it follows every dependency edge the
// lockfile records, resolving each edge twice: in the lockfile, by npm's path keys, and on
// disk, by the node_modules walk Node performs from the dependent's REAL directory. The two
// resolutions must agree on the version at every edge, and every node reachable that way
// must exist. Deliberately layout-agnostic: npm hoists everything to the root and nub's
// isolated linker keeps only direct dependencies there, and both satisfy the same edges, so
// a green result says "the lockfile's graph is met", not "the trees are identical".
//
// An importer's declared dependency the lockfile does not pin is a failure: that is the
// package.json/lockfile drift `npm ci` refuses. Optional dependencies are skipped (a
// platform mismatch drops them legitimately). A workspace link is judged by the member
// directory's own manifest, not by the version the lockfile recorded for it: npm links the
// directory as it is and `npm ci` never compares the two (socket.io's lockfile records
// engine.io at 6.6.9 while the member is at 6.6.10).
//
// A required peer must be present and satisfy the dependent's declared range; its exact
// version is reported, not judged. npm resolves a peer by where hoisting happened to place
// the dependent, so a plugin hoisted to the root sees the root's typescript even when the
// member that uses it pins another; nub resolves a peer from the dependent's own context.
// Both meet the package's declared contract, and only the regular dependency graph is what
// the lockfile pins. The walk starts from the dependent's real directory, so a peer nub
// links beside a package in its store counts exactly as a hoisted one does.
//
// One tolerance, by construction of the store: npm's lockfile can place the same
// name@version twice with different transitive resolutions (http-errors@1.6.3 under `send`
// resolving statuses@1.4.0, under `serve-index` statuses@1.5.0). A content-addressed store
// keys a package by name@version, so nub — like `pnpm import` — keeps one of those
// resolutions for both placements. An edge whose version is what ANOTHER placement of the
// same dependent resolves to is accepted and counted as collapsed; a version no placement
// resolves to is a mismatch.
//
//   node tests/npm-corpus/check-tree.mjs <project-dir>
import { existsSync, readFileSync, realpathSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

// Real path, so the walk below can tell "inside the project" from "outside" after every
// package directory has been resolved through its symlinks (/tmp is one on macOS).
const root = realpathSync(resolve(process.argv[2] ?? "."));
const lock = JSON.parse(readFileSync(join(root, "package-lock.json"), "utf8"));
if (!lock.packages) {
  console.error(`check-tree: lockfileVersion ${lock.lockfileVersion} carries no "packages" map`);
  process.exit(2);
}
const packages = lock.packages;
const readVersion = (file) => JSON.parse(readFileSync(file, "utf8")).version;

// Importers are the root and every workspace member npm links at the root. A member entry
// nothing links is a leftover from a renamed or removed package (socket.io's lockfile still
// carries `packages/socket.io-clustered-engine`); npm does not install it, so nor does this.
// A link whose target sits inside a node_modules directory is a package linking to itself
// (puppeteer's lockfile has one under a nested browserslist), not a workspace.
const linked = new Set(
  Object.values(packages)
    .filter((entry) => entry.link && entry.resolved && !entry.resolved.split("/").includes("node_modules"))
    .map((entry) => entry.resolved),
);
const importers = Object.keys(packages).filter((key) => key === "" || linked.has(key));

// The lockfile keys mirror the walk: <from>/node_modules/<name>, then each ancestor of <from>.
function lockResolve(from, name) {
  let dir = from;
  for (;;) {
    const key = dir === "" ? `node_modules/${name}` : `${dir}/node_modules/${name}`;
    if (packages[key]) return { key, entry: packages[key] };
    if (dir === "") return null;
    const cut = dir.lastIndexOf("/");
    dir = cut === -1 ? "" : dir.slice(0, cut);
  }
}
// A workspace link's version is the member directory's, as installed; every other entry
// carries its own.
function lockVersion({ key, entry }) {
  if (!entry.link) return { key, version: entry.version };
  try {
    return { key: entry.resolved, version: readVersion(join(root, entry.resolved, "package.json")) };
  } catch {
    return { key: entry.resolved, version: packages[entry.resolved]?.version };
  }
}
// The subset of semver this needs: the ranges packages declare on their peers.
function satisfies(version, range) {
  const v = version.match(/^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?/);
  if (!v) return false;
  const [maj, min, pat] = [Number(v[1]), Number(v[2]), Number(v[3])];
  const cmp = (a, b) => a[0] - b[0] || a[1] - b[1] || a[2] - b[2];
  const parse = (t) => {
    const m = t.match(/^(\d+|x|\*)(?:\.(\d+|x|\*))?(?:\.(\d+|x|\*))?/);
    if (!m) return null;
    const num = (x) => (x === undefined || x === "x" || x === "*" ? null : Number(x));
    return [num(m[1]), num(m[2]), num(m[3])];
  };
  const clause = (c) => {
    c = c.trim();
    if (c === "" || c === "*" || c === "x") return true;
    const hyphen = c.match(/^(\S+)\s+-\s+(\S+)$/);
    if (hyphen) return clause(">=" + hyphen[1]) && clause("<=" + hyphen[2]);
    return c.split(/\s+/).every((part) => {
      const m = part.match(/^(\^|~|>=|<=|>|<|=)?v?(.+)$/);
      const op = m[1] ?? "";
      const p = parse(m[2]);
      if (!p) return false;
      const [a, b, d] = p;
      const lo = [a ?? 0, b ?? 0, d ?? 0];
      const cur = [maj, min, pat];
      if (op === ">=") return cmp(cur, lo) >= 0;
      if (op === ">") return cmp(cur, lo) > 0;
      if (op === "<=") return cmp(cur, lo) <= 0;
      if (op === "<") return cmp(cur, lo) < 0;
      let hi;
      if (op === "^") hi = a > 0 ? [a + 1, 0, 0] : b > 0 ? [0, b + 1, 0] : [0, 0, (d ?? 0) + 1];
      else if (op === "~") hi = b === null ? [a + 1, 0, 0] : [a, b + 1, 0];
      else if (b === null) hi = [a + 1, 0, 0];
      else if (d === null) hi = [a, b + 1, 0];
      else return cmp(cur, lo) === 0;
      return cmp(cur, lo) >= 0 && cmp(cur, hi) < 0;
    });
  };
  // "`>= 4.3.x`" is a common spelling: fold the space after an operator before splitting.
  return range.replace(/([<>=~^]+)\s+/g, "$1").split("||").some(clause);
}
// The walk Node performs from `fromDir`, which must be a REAL directory: under nub's isolated
// layout a package's dependencies and peers sit beside it in the store, not under the root.
// The walk stops at the project root, and at the filesystem root for a store kept outside it.
function diskResolve(fromDir, name) {
  let dir = fromDir;
  for (;;) {
    const pkgDir = join(dir, "node_modules", name);
    const manifest = join(pkgDir, "package.json");
    if (existsSync(manifest)) {
      try {
        return { path: manifest, dir: realpathSync(pkgDir), version: readVersion(manifest) };
      } catch (error) {
        return { path: manifest, dir: pkgDir, version: undefined, error: error.message };
      }
    }
    const parent = dirname(dir);
    if (dir === root || parent === dir) return null;
    dir = parent;
  }
}

// (dependent name@version, dependency name) → every version the lockfile resolves it to
// across the dependent's placements; more than one means npm placed it twice.
const placements = new Map();
for (const key of Object.keys(packages)) {
  const entry = packages[key];
  if (key === "" || entry.link || !entry.version) continue;
  const name = entry.name ?? key.slice(key.lastIndexOf("node_modules/") + "node_modules/".length);
  const declared = [
    ...Object.keys(entry.dependencies ?? {}),
    ...Object.keys(entry.optionalDependencies ?? {}),
    ...Object.keys(entry.peerDependencies ?? {}),
  ];
  for (const dep of declared) {
    const pinned = lockResolve(key, dep);
    if (!pinned) continue;
    const id = `${name}@${entry.version}\0${dep}`;
    if (!placements.has(id)) placements.set(id, new Set());
    placements.get(id).add(lockVersion(pinned).version);
  }
}

const missing = [];
const mismatched = [];
const unpinned = [];
let collapsed = 0;
let peersByContext = 0;
let edges = 0;
let nodes = 0;
let optionalSkipped = 0;
let transitiveUnpinned = 0;

// Breadth-first over (lockfile node, real directory). Every importer is queued before any
// package is processed, so a workspace member reached as another importer's dependency is
// still checked as an importer — with its devDependencies — and only once.
const queue = [];
for (const importer of importers) {
  try {
    queue.push({ lockKey: importer, realDir: realpathSync(join(root, importer)), isImporter: true });
  } catch {
    missing.push(`${importer}: workspace directory absent`);
  }
}
const seen = new Set();
const enqueue = (lockKey, realDir) => queue.push({ lockKey, realDir, isImporter: false });

// Every edge the lockfile records for `lockKey`, checked from `realDir`.
function check(lockKey, realDir, isImporter) {
  const id = `${lockKey}\0${realDir}`;
  if (seen.has(id)) return;
  seen.add(id);
  nodes += 1;
  const entry = packages[lockKey];
  const where = lockKey === "" ? "." : lockKey;
  const optional = new Set(Object.keys(entry.optionalDependencies ?? {}));
  const peerMeta = entry.peerDependenciesMeta ?? {};
  // A name declared as both a dependency and a peer is a peer: the package takes whichever
  // copy its dependent context provides, and that is the contract judged here.
  const declared = new Map();
  for (const name of Object.keys(entry.dependencies ?? {})) declared.set(name, "dependency");
  if (isImporter) for (const name of Object.keys(entry.devDependencies ?? {})) declared.set(name, "devDependency");
  for (const name of Object.keys(entry.peerDependencies ?? {})) {
    if (peerMeta[name]?.optional) declared.delete(name);
    else declared.set(name, "peer");
  }
  for (const [name, kind] of declared) {
    if (optional.has(name)) {
      optionalSkipped += 1;
      continue;
    }
    const pinned = lockResolve(lockKey, name);
    if (!pinned) {
      // npm's lockfile records every peer it installed; one it did not is the dependent's
      // problem under npm too, and nothing here can say what version to expect.
      if (kind === "peer" || !isImporter) transitiveUnpinned += 1;
      else unpinned.push(`${where}: ${name} (${kind})`);
      continue;
    }
    const want = lockVersion(pinned);
    const have = diskResolve(realDir, name);
    edges += 1;
    if (!have) {
      missing.push(`${where} → ${name}@${want.version ?? "?"} (${kind})`);
      continue;
    }
    if (want.version && have.version !== want.version) {
      const dependent = lockKey === "" ? null : `${entry.name ?? lockKey.slice(lockKey.lastIndexOf("node_modules/") + "node_modules/".length)}@${entry.version}`;
      const elsewhere = dependent ? placements.get(`${dependent}\0${name}`) : undefined;
      if (kind === "peer" && have.version && satisfies(have.version, entry.peerDependencies[name])) {
        peersByContext += 1;
      } else if (elsewhere && elsewhere.size > 1 && elsewhere.has(have.version)) {
        collapsed += 1;
      } else {
        mismatched.push(`${where} → ${name}: lockfile ${want.version}, found ${have.version ?? have.error} at ${have.path}${kind === "peer" ? " (peer, out of range)" : ""}`);
        continue;
      }
      // Continue the walk from the lockfile's node for this dependent: the disk copy is
      // another placement of the same package, whose own edges the lockfile pins elsewhere.
      const found = lockResolve("", name);
      if (found && lockVersion(found).version === have.version) enqueue(found.key, have.dir);
      continue;
    }
    enqueue(want.key, have.dir);
  }
}

for (let i = 0; i < queue.length; i += 1) {
  const { lockKey, realDir, isImporter } = queue[i];
  check(lockKey, realDir, isImporter);
}

console.log(
  `check-tree: ${importers.length} importer(s), ${nodes} package(s) reached over ${edges} lockfile edge(s); ` +
    `${missing.length} missing, ${mismatched.length} at another version, ${unpinned.length} declared but unpinned` +
    ` (skipped ${optionalSkipped} optional, ${transitiveUnpinned} unpinned transitive or peer; ${collapsed} collapsed placement(s), ${peersByContext} peer(s) resolved by context)`,
);
for (const line of missing) console.log(`  missing     ${line}`);
for (const line of mismatched) console.log(`  mismatched  ${line}`);
for (const line of unpinned) console.log(`  unpinned    ${line}`);
process.exit(missing.length + mismatched.length + unpinned.length === 0 ? 0 : 1);
