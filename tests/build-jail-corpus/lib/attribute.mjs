// attribute.mjs — map a delta path to the package that owns it.
//
// THIS IS WHAT MAKES A SHARED-REPO SHARD WORKABLE. In a shard carrying hundreds
// of dependencies, "which package did this?" cannot come from the log: nub does
// NOT frame lifecycle-script output per package. Measured on a real run — the
// approve-builds section interleaves raw `gyp info …` lines with no package
// header and no per-script rc. The only package-naming line is the opt-out
// warning, and that appears solely in the unconfined arm.
//
// The filesystem answers it instead, structurally. nub uses an isolated
// (pnpm-shaped) store, so every package materialises at
//     <store>/<name>@<version>-<hash>/node_modules/<name>/…
// and — the load-bearing part — its build output lands THERE, not in the
// project. Measured: bcrypt's `build/Release/bcrypt_lib.node` and all its `.o`
// files were written under `store/bcrypt@6.0.0-a5bcef9752d349b4/`. So the path
// itself carries an exact name@version key, and attribution is a parse rather
// than an inference.
//
// ⭐ AND THE INTERESTING CASE FALLS OUT FOR FREE: a write that lands OUTSIDE the
// writer's own store cell is precisely the interaction/attack shape this study
// exists to surface — one dependency planting `$HOME/.npmrc` for another to
// read, or two packages contending on a shared CAS cell. Those are unownable by
// path, so they are reported as UNATTRIBUTED, which is a finding, not noise.

// Store cell naming, as MEASURED rather than assumed:
//   bcrypt@6.0.0-a5bcef9752d349b4
//   @prisma+client@6.19.3_prisma@6.19.3_-5a166de6171ae5ba
// A scoped name mangles `/` to `+`, and a peer-dependent package carries a
// `_peer@ver_` suffix before the content hash. A regex written for the simple
// shape silently fails to attribute every scoped package — which is how eight
// installed packages first reported as NOT-INSTALLED.
const STORE_CELL = /(?:^|\/)store\/(.+?)@([^/@]+?)(?:_[^/]*_)?-[0-9a-f]{8,}\/node_modules\/(.+)$/;
// THERE ARE TWO STORE-SHAPED LOCATIONS, and missing the second produced a false
// NEVER-RAN on Prisma while its generated client (carrying the run nonce) sat on
// disk. The global CAS store is content-hashed and lives under HOME; the
// PROJECT virtual store is NOT hashed and is where per-project generated output
// actually lands:
//   proj:node_modules/.store/@prisma+client@6.19.3_prisma@6.19.3/node_modules/.prisma/client/…
const PROJ_CELL = /^node_modules\/\.store\/(.+?)@([^/@_]+)(?:_[^/]*)?\/node_modules\/(.+)$/;
const unmangle = (n) => n.replace(/\+/g, '/');

// Store bookkeeping that is nub's own, not any package's content.
const STORE_META = /(?:^|\/)store\/(?:\.tmp|\.index|_locks|index-v\d)/;

export function attribute(rootAndRel) {
  const [root, ...rest] = rootAndRel.split(':');
  const rel = rest.join(':');

  if (STORE_META.test(rel)) return { owner: null, kind: 'pm-meta' };

  const m = rel.match(STORE_CELL) || (root === 'proj' || root === 'nm' ? rel.match(PROJ_CELL) : null);
  if (m) {
    const [, rawName, version, inner] = m;
    const name = unmangle(rawName);
    // `inner` is `<name>/…` for the package's own content, or
    // `<otherpkg>/…` when the cell holds a dependency link — the latter is a
    // materialisation detail, not this package's work.
    // A cell's entire node_modules is that package's territory: Prisma writes
    // its generated client to `.prisma/client`, a SIBLING of `@prisma/client`
    // inside the same cell. Requiring the inner path to start with the package
    // name discarded exactly that output.
    const ownSelf = inner.startsWith(name + '/') || inner === name || inner.startsWith('.');
    return {
      owner: `${name}@${version}`,
      kind: ownSelf ? 'own-package-dir' : 'cell-dependency-link',
      // Keep the FULL cell-relative path. Slicing off the package-name prefix
      // silently corrupted sibling output: `.prisma/client/` and `@prisma/client/`
      // are both 15 characters, so the slice produced a plausible-looking
      // `index.js` that resolved to a path which does not exist, and the nonce
      // check then reported NOT FOUND on a file that was plainly on disk.
      inner,
    };
  }

  if (root === 'proj') return { owner: null, kind: 'project-file' };
  if (root === 'home') return { owner: null, kind: 'home' };
  if (root === 'tmp') return { owner: null, kind: 'tmp' };
  return { owner: null, kind: 'other' };
}

// Group a delta's entries by owning package. Everything unownable lands in
// UNATTRIBUTED, which the caller must surface rather than discard: a project
// write, a $HOME write, or a write into someone else's cell is the whole point
// of running a shared shard instead of isolated fixtures.
export function groupByOwner(delta) {
  const byOwner = new Map();
  const unattributed = { 'project-file': [], home: [], tmp: [], other: [], 'cell-dependency-link': [] };
  for (const cls of ['created', 'modified', 'deleted']) {
    for (const e of delta[cls] || []) {
      const k = `${e.root}:${e.rel}`;
      const a = attribute(k);
      if (a.kind === 'pm-meta') continue;
      if (a.owner && a.kind === 'own-package-dir') {
        if (!byOwner.has(a.owner)) byOwner.set(a.owner, { created: [], modified: [], deleted: [] });
        byOwner.get(a.owner)[cls].push(a.inner);
      } else {
        (unattributed[a.kind] ||= []).push({ cls, path: k, owner: a.owner ?? null });
      }
    }
  }
  return { byOwner, unattributed };
}

// Distinguish SCRIPT OUTPUT from PM MATERIALISATION inside the same window.
//
// During `approve-builds` nub may lazily materialise a whole toolchain (the
// node-gyp bootstrap pulled tar, undici, semver, … into the store), and those
// cells' files are attributable by path but are NOT any lifecycle script's work.
//
// The discriminator is NOT mtime. That hypothesis was tested and REFUTED: nub's
// extractor stamps mtime=now, so a freshly extracted `undici/lib/global.js` and
// a freshly compiled `bcrypt.o` are both ~0.1 h old and indistinguishable.
//
// What does separate them is whether the package's store CELL existed before the
// window at all. A cell that appeared wholesale during the window is a
// materialisation; a cell that already existed and then gained files was written
// into by something that ran.
export function cellsPresentIn(records) {
  const cells = new Set();
  for (const r of records) {
    const m = `${r.root}:${r.rel}`.match(STORE_CELL);
    if (m) cells.add(`${unmangle(m[1])}@${m[2]}`);
  }
  return cells;
}
