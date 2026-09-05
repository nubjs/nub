// Behaviour a compiled artifact owes the same file run through nub, checked by
// comparing the two rather than by asserting a number this script decided.
//
// Every case here was a real divergence at some point in this shape's history, and
// each is silent: the artifact runs, prints what you expect, and exits with the
// wrong status — or quietly loses a global.
//
// usage: node semantics.mjs --nub <path> [--out-dir <dir>] [--modern-node-only 1]
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const args = new Map();
for (let i = 2; i < process.argv.length; i += 2) args.set(process.argv[i].replace(/^--/, ""), process.argv[i + 1]);
const nub = resolve(args.get("nub"));
const outDir = args.get("out-dir") ? resolve(args.get("out-dir")) : mkdtempSync(join(tmpdir(), "nub-sea-semantics-"));
mkdirSync(outDir, { recursive: true });

let failed = 0;
const fail = (msg) => { console.error(`FAIL: ${msg}`); failed += 1; };

const CASES = [
  {
    name: "unsettled top-level await",
    // Node exits 13 for an entry whose evaluation never settles. A loader that
    // starts the entry and does not observe the promise exits 0 instead.
    source: 'console.log("before"); await new Promise(() => {}); console.log("never");',
    node: "26.7.0",
  },
  {
    name: "throwing entry",
    source: 'throw new Error("boom");',
    node: "26.7.0",
  },
  {
    name: "throwing entry under --unhandled-rejections=warn",
    // A failed ESM ENTRY is an uncaught exception in Node, not an unhandled
    // rejection, so this mode must not turn a failure into a clean exit.
    source: 'throw new Error("boom");',
    node: "26.7.0",
    env: { NODE_OPTIONS: "--unhandled-rejections=warn" },
  },
  {
    name: "explicit process.exitCode",
    source: "process.exitCode = 7;",
    node: "26.7.0",
  },
  {
    name: "localStorage with a run-time storage file",
    // The artifact's Web Storage decision is made when the blob is written, where
    // the user's NODE_OPTIONS does not exist yet. On a Node whose localStorage
    // throws without a storage file, supplying one at run time has to win.
    source: 'let s; try { s = typeof localStorage; } catch (e) { s = "throws"; } console.log("localStorage:", s);',
    node: "22.20.0",
    env: { NODE_OPTIONS: `--localstorage-file=${join(outDir, "store.db")}` },
    // Node 22 ships none of Temporal, URLPattern or Float16Array, so compiling
    // for it pulls the polyfill packages out of `runtime/node_modules` — which a
    // fresh checkout does not have. Skipped where the runtime is not staged.
    needsPolyfills: true,
  },
];

const cases = args.get("modern-node-only") ? CASES.filter((c) => !c.needsPolyfills) : CASES;

for (const [index, testCase] of cases.entries()) {
  const dir = join(outDir, `case-${index}`);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "package.json"), '{ "name": "case", "version": "1.0.0", "type": "module" }\n');
  writeFileSync(join(dir, ".node-version"), testCase.node);
  writeFileSync(join(dir, "app.mjs"), `${testCase.source}\n`);

  const env = { ...process.env, ...testCase.env };
  const control = spawnSync(nub, [join(dir, "app.mjs")], { encoding: "utf8", env, cwd: dir });
  const out = join(dir, "artifact");
  const built = spawnSync(nub, ["compile", join(dir, "app.mjs"), "--out", out], { encoding: "utf8", cwd: dir });
  if (built.status !== 0) {
    fail(`${testCase.name}: compile exited ${built.status}\n${built.stderr ?? ""}`);
    continue;
  }
  const ran = spawnSync(out, [], { encoding: "utf8", env, cwd: dir });

  const label = `${testCase.name}: exit ${ran.status} (nub gives ${control.status})`;
  if (ran.status !== control.status) fail(label);
  else console.log(`  ok — ${label}`);

  const wanted = (control.stdout ?? "").trim();
  const carried = (ran.stdout ?? "").trim();
  if (carried !== wanted) fail(`${testCase.name}: printed ${JSON.stringify(carried)}, nub printed ${JSON.stringify(wanted)}`);
}

if (failed > 0) process.exit(1);
console.log(`OK ${cases.length} semantics cases match nub`);
