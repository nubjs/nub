#!/usr/bin/env node
// Diff the running Node's --experimental-* flag inventory against a committed snapshot,
// and FAIL LOUDLY on any drift. Two callers, one comparison:
//
//   • snapshot-check (per-PR, Node pinned to the snapshot's version) — must be identical;
//     a diff means the snapshot or the inventory tool was edited wrong. Deterministic.
//   • nightly-drift (scheduled, Node nightly / RC) — a diff means Node added or removed an
//     experimental flag since the snapshot; the report tells a human what to vet and,
//     if it belongs, hand-add to the feature matrix (nub never auto-injects an unvetted flag).
//
// Usage:
//   node scripts/check-node-flag-drift.mjs --snapshot <path>     # compare, exit 1 on drift
//   node scripts/check-node-flag-drift.mjs --snapshot <path> --update   # rewrite snapshot from this Node
//
// The comparison uses the CURRENT `node` (process.execPath) — pick the Node by putting it
// first on PATH / via setup-node before invoking this. Needs no flags itself; it spawns the
// inventory tool with --expose-internals.
import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const TYPE_NAME = {
  0: "kNoOp",
  1: "kV8Option",
  2: "kBoolean",
  3: "kInteger",
  4: "kUInteger",
  5: "kString",
  6: "kHostPort",
  7: "kStringList",
};

function parseArgs(argv) {
  let snapshot = null;
  let update = false;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--snapshot") snapshot = argv[++i];
    else if (argv[i] === "--update") update = true;
    else if (argv[i] === "--help" || argv[i] === "-h") {
      process.stdout.write("usage: check-node-flag-drift.mjs --snapshot <path> [--update]\n");
      process.exit(0);
    }
  }
  if (!snapshot) {
    process.stderr.write("check-node-flag-drift: --snapshot <path> is required\n");
    process.exit(2);
  }
  return { snapshot, update };
}

// Capture THIS Node's inventory by running the inventory tool with --expose-internals.
function captureCurrentInventory() {
  const here = dirname(fileURLToPath(import.meta.url));
  const tool = join(here, "node-flag-inventory.mjs");
  const res = spawnSync(
    process.execPath,
    ["--no-warnings", "--expose-internals", tool],
    { encoding: "utf8", maxBuffer: 16 * 1024 * 1024 },
  );
  if (res.status !== 0) {
    process.stderr.write(
      `check-node-flag-drift: failed to capture inventory on ${process.version}\n` +
        (res.stderr || "") +
        "\nThe flag introspection is unavailable on this build — a human must confirm whether\n" +
        "the internalBinding('options') shape changed and update the tool.\n",
    );
    process.exit(2);
  }
  return JSON.parse(res.stdout);
}

const { snapshot: snapshotPath, update } = parseArgs(process.argv.slice(2));
const current = captureCurrentInventory();

if (update) {
  writeFileSync(snapshotPath, JSON.stringify(current, null, 2) + "\n");
  process.stdout.write(
    `Updated ${snapshotPath} from ${current.nodeVersion} ` +
      `(${Object.keys(current.flags).length} experimental flags).\n`,
  );
  process.exit(0);
}

let snapshot;
try {
  snapshot = JSON.parse(readFileSync(snapshotPath, "utf8"));
} catch (err) {
  process.stderr.write(`check-node-flag-drift: cannot read snapshot ${snapshotPath}: ${err?.message ?? err}\n`);
  process.exit(2);
}

const shape = ([t, e]) => `${TYPE_NAME[t] ?? t}, env=${e}`;
const cur = current.flags;
const snap = snapshot.flags;

const added = Object.keys(cur).filter((k) => !(k in snap)).sort();
const removed = Object.keys(snap).filter((k) => !(k in cur)).sort();
const changed = Object.keys(cur)
  .filter((k) => k in snap && (cur[k][0] !== snap[k][0] || cur[k][1] !== snap[k][1]))
  .sort();

if (added.length === 0 && removed.length === 0 && changed.length === 0) {
  process.stdout.write(
    `No flag drift. ${current.nodeVersion} matches the snapshot ` +
      `(${snapshot.nodeVersion}, ${Object.keys(snap).length} experimental flags).\n`,
  );
  process.exit(0);
}

// Drift → loud, actionable report on stderr, non-zero exit.
const lines = [
  "EXPERIMENTAL-FLAG DRIFT DETECTED",
  `  snapshot: ${snapshot.nodeVersion} (${snapshotPath})`,
  `  current:  ${current.nodeVersion}`,
  "",
];
for (const k of added) lines.push(`  + ADDED    ${k}  [${shape(cur[k])}]`);
for (const k of removed) lines.push(`  - REMOVED  ${k}  [was ${shape(snap[k])}]`);
for (const k of changed) lines.push(`  ~ CHANGED  ${k}  [${shape(snap[k])} -> ${shape(cur[k])}]`);
lines.push(
  "",
  "An ADDED flag is a new experimental Node capability — vet it and, if nub should enable",
  "it, hand-add a band to crates/nub-core/src/node/feature_matrix.rs. A REMOVED flag that",
  "the matrix still injects would crash on this Node — tighten its band. Once vetted,",
  "refresh the snapshot: node scripts/check-node-flag-drift.mjs --snapshot " + snapshotPath + " --update",
  "",
);
process.stderr.write(lines.join("\n") + "\n");
process.exit(1);
