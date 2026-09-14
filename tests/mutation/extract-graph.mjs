#!/usr/bin/env node
// Semantic dependency-graph extractor for the mutation differential.
//
// Given a project directory, read its pnpm-format lockfile — pnpm-lock.yaml, or
// nub.lock when there is none, since the two share one format — and emit a
// NORMALIZED, order-insensitive view of the resolved graph:
//
//   {
//     "format": "pnpm",
//     "direct":   { "<name>": "<declared-spec>", ... },   // root importer deps
//     "resolved": { "<name>@<version>": <count>, ... }     // every resolved pkg
//   }
//
// WHY this shape is the semantic signal (and byte-`cmp` is not):
//   - `resolved` is the MULTISET of every concrete package@version in the
//     lockfile. It captures the three things a mutation changes and the three
//     bug classes the suite hunts:
//       * add  (M.1)  — the new dep + its transitives APPEAR in the set.
//       * dedup (M.3) — whether a shared transitive collapses to one version
//                       or keeps two shows up as one-vs-two keys in the set.
//       * prune (M.5) — removed/kept transitives are present/absent.
//   - `direct` is the root importer's declared specifiers (name -> range). It
//     captures the manifest-side mutation: `add pkg@^1` must write `^1`
//     verbatim, `remove` must drop the entry.
//
// The comparator (compare-graphs.mjs) diffs two of these JSON blobs for
// equality, ignoring key ordering.
//
// Usage:  extract-graph.mjs <project-dir>

import fs from "node:fs";
import path from "node:path";

function die(msg) {
  process.stderr.write(`extract-graph: ${msg}\n`);
  process.exit(2);
}

const args = process.argv.slice(2);
if (args.length !== 1) die("usage: extract-graph.mjs <project-dir>");
const dir = args[0];

const bump = (obj, key) => {
  obj[key] = (obj[key] || 0) + 1;
};

// ── pnpm: pnpm-lock.yaml ──────────────────────────────────────────────────
// `importers.<.>.{dependencies,devDependencies,optionalDependencies}` carries
// the root direct specs (each entry: `name: { specifier, version }`). The
// flat `packages:` section keys are `name@version` (scoped: `@scope/name@ver`,
// peer-suffixed: `name@ver(peerdep@x)` — we strip the peer suffix and keep the
// base name@version). A tiny hand YAML walk avoids a yaml dep (the structure we
// need is shallow + regular).
function extractPnpm(text) {
  const direct = {};
  const resolved = {};
  const lines = text.split("\n");

  // Walk the `importers:` block, root importer `.` only (the `.:` two-space key).
  // Direct deps live under `dependencies:` / `devDependencies:` /
  // `optionalDependencies:` as `name:` then `specifier: <spec>` / `version:`.
  let i = 0;
  for (; i < lines.length; i++) if (/^importers:\s*$/.test(lines[i])) break;
  if (i < lines.length) {
    i++;
    // find the root `  .:` importer
    for (; i < lines.length; i++) {
      if (/^\S/.test(lines[i])) break; // left the importers block
      if (/^ {2}(['"]?)\.\1:\s*$/.test(lines[i])) {
        i++;
        // inside root importer: 4-space dep-bucket headers, 6-space names
        let bucket = null;
        for (; i < lines.length; i++) {
          const l = lines[i];
          if (/^ {0,3}\S/.test(l) || /^ {2}\S/.test(l)) {
            i--;
            break;
          } // dedent out of importer
          let m;
          if ((m = l.match(/^ {4}(dependencies|devDependencies|optionalDependencies):\s*$/))) {
            bucket = m[1];
          } else if (bucket && (m = l.match(/^ {6}(\S+?):\s*$/))) {
            const name = m[1].replace(/^['"]|['"]$/g, "");
            // next line(s): specifier: <spec>
            let spec = "*";
            for (let j = i + 1; j < lines.length && /^ {8}/.test(lines[j]); j++) {
              const sm = lines[j].match(/^ {8}specifier:\s*(.+?)\s*$/);
              if (sm) {
                spec = sm[1].replace(/^['"]|['"]$/g, "");
                break;
              }
            }
            direct[name] = spec;
          }
        }
        break;
      }
    }
  }

  // `packages:` keys -> name@version multiset.
  let inPkgs = false;
  for (const l of lines) {
    if (/^packages:\s*$/.test(l)) {
      inPkgs = true;
      continue;
    }
    if (inPkgs) {
      if (/^\S/.test(l)) break; // next top-level section (snapshots:)
      const m = l.match(/^ {2}(\S.*?):\s*$/);
      if (m) {
        let key = m[1].replace(/^['"]|['"]$/g, "");
        key = key.replace(/\([^)]*\)/g, ""); // strip peer-dep suffix
        resolved[key] = (resolved[key] || 0) + 1;
      }
    }
  }
  return { format: "pnpm", direct, resolved };
}

const lockPath = ["pnpm-lock.yaml", "nub.lock"].map((f) => path.join(dir, f)).find((f) => fs.existsSync(f));
if (!lockPath) die(`no lockfile (pnpm-lock.yaml / nub.lock) in ${dir}`);

// A project that pins pnpm gets a lockfile of two YAML documents from pnpm 12:
// the package manager's own (`packageManagerDependencies`) first, then the
// project's. The project graph is the last document.
const documents = fs.readFileSync(lockPath, "utf8").split(/^---$/m).filter((doc) => /^importers:/m.test(doc));
if (documents.length === 0) die(`no importers in ${lockPath}`);
const out = extractPnpm(documents[documents.length - 1]);
process.stdout.write(JSON.stringify(out, null, 2) + "\n");
