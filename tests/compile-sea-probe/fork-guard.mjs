// The single-executable shape's last line of defence against a fork bomb.
//
// A payload that reaches `child_process`/`cluster` is supposed to keep the
// launcher, which has a real Node to hand a fork. That decision is made by
// reading the emitted chunks for what they RESOLVE, which is syntactic and so a
// heuristic: it follows a require through a rename and through an alias binding,
// and it cannot follow one stored on an object and called through a property.
// The fixture below is exactly that shape, and it really does select the
// single-executable container — which is why the loader replaces
// `child_process.fork` with one that throws.
//
// Without the guard the failure is unbounded rather than merely wrong: a fork
// spawns `process.execPath`, which is the artifact, and Node discards a
// single-executable's `argv[1]`, so the child re-runs the whole application and
// forks again. This script is written so that a REGRESSION cannot run away —
// each generation carries a counter and the fixture stops itself at three.
//
// usage: node fork-guard.mjs --nub <path> [--out-dir <dir>]
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const args = new Map();
for (let i = 2; i < process.argv.length; i += 2) args.set(process.argv[i].replace(/^--/, ""), process.argv[i + 1]);
const nub = resolve(args.get("nub"));
const dir = args.get("out-dir") ? resolve(args.get("out-dir")) : mkdtempSync(join(tmpdir(), "nub-sea-fork-guard-"));
mkdirSync(dir, { recursive: true });

// CommonJS, because `require` is what the scan follows and this has to reach it
// by a route the scan cannot. The object property is the route; the generation
// cap is the containment.
const FIXTURE = [
  'const GEN = Number(process.env.GEN || "0");',
  'if (GEN > 2) { console.log("gen", GEN, "STOPPING"); process.exit(9); }',
  "globalThis.__holder = { r: require };",
  'const cp = globalThis.__holder.r("node:child_process");',
  "try {",
  "  cp.fork(__filename, [], { env: { ...process.env, GEN: String(GEN + 1) } });",
  '  console.log("FORKED");',
  "} catch (error) {",
  '  console.log("REFUSED:", error.message);',
  "}",
].join("\n");

writeFileSync(join(dir, "package.json"), '{ "name": "fork-guard", "version": "1.0.0", "private": true }\n');
writeFileSync(join(dir, ".node-version"), "26.7.0");
writeFileSync(join(dir, "app.js"), `${FIXTURE}\n`);

const out = join(dir, "artifact");
const built = spawnSync(nub, ["compile", join(dir, "app.js"), "--out", out], { encoding: "utf8", cwd: dir });
if (built.status !== 0) {
  console.error(`FAIL: compile exited ${built.status}\n${built.stderr ?? ""}`);
  process.exit(1);
}

// The premise of the whole case. If the scan ever learns to follow this shape the
// payload declines instead, the artifact is a launcher, and the guard is not the
// thing being tested any more — so say that rather than passing quietly.
//
// Both streams, because which one carries the summary is the renderer's business
// and not this script's: reading only stdout reported the premise broken while
// the same build was in fact producing a single-executable artifact.
const summary = `${built.stdout ?? ""}${built.stderr ?? ""}`;
if (!summary.includes("a single-executable application")) {
  console.error("SKIP: this fixture no longer selects the single-executable container");
  console.error(summary);
  process.exit(0);
}

const ran = spawnSync(out, [], { encoding: "utf8", cwd: dir });
const stdout = ran.stdout ?? "";
let failed = 0;
const fail = (message) => {
  console.error(`FAIL: ${message}`);
  failed += 1;
};

if (!stdout.includes("REFUSED:")) fail(`fork was not refused. stdout: ${JSON.stringify(stdout)}`);
if (stdout.includes("FORKED")) fail("fork succeeded, so the child re-ran the application");
if (stdout.includes("gen 1")) fail("a second generation started, which is the fork bomb this guards");
if (ran.status !== 0) fail(`artifact exited ${ran.status}, expected 0`);

if (failed > 0) process.exit(1);
console.log("OK fork is refused inside a single-executable artifact");
