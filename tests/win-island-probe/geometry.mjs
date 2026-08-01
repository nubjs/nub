/**
 * Windows-only probe: reproduce the module-resolution geometry a compiled nub
 * artifact creates, without building nub.
 *
 * The launcher hands Node an entry path that came from Rust `fs::canonicalize`,
 * which on Windows returns the VERBATIM spelling (`\\?\C:\...`). Node's realpath
 * for the same file returns the ordinary spelling. This probe drives both
 * spellings through the real loader and reports where a bare `C:` appears.
 */
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";

if (process.platform !== "win32") {
  console.error("this probe only means anything on win32");
  process.exit(2);
}

const root = mkdtempSync(join(tmpdir(), "winisl-"));
const app = join(root, "app");
const island = join(app, "__nub_native", "deadbeefdeadbeef", "node_modules", "pkg", "lib");
const foreign = join(root, "foreign-cwd");
mkdirSync(island, { recursive: true });
mkdirSync(foreign, { recursive: true });

const islandRel = "./__nub_native/deadbeefdeadbeef/node_modules/pkg/lib/addon.node";
// A plain-text ".node" resolves fine and then fails at dlopen; that distinct
// failure is the signal that RESOLUTION succeeded.
writeFileSync(join(island, "addon.node"), "not a real dll");

writeFileSync(
  join(app, "boot.cjs"),
  `const { createRequire } = require("node:module");
Object.defineProperty(process, Symbol.for("nub.compile.bootstrap"), {
  value: Object.freeze({ createRequire }),
  enumerable: false, configurable: false, writable: false,
});
process._rawDebug("[boot] __filename=" + __filename);
`,
);

writeFileSync(
  join(app, "entry.mjs"),
  `const record = process[Symbol.for("nub.compile.bootstrap")];
process._rawDebug("[child] import.meta.url = " + import.meta.url);
process._rawDebug("[child] cwd            = " + process.cwd());
process._rawDebug("[child] argv1          = " + process.argv[1]);
process._rawDebug("[child] =C:            = " + JSON.stringify(process.env["=C:"]));
process._rawDebug("[child] =D:            = " + JSON.stringify(process.env["=D:"]));
const req = record.createRequire(import.meta.url);
process._rawDebug("[child] require.filename-dir probe:");
try {
  process._rawDebug("[child] resolve -> " + req.resolve(${JSON.stringify(islandRel)}));
} catch (e) {
  process._rawDebug("[child] RESOLVE FAILED " + e.code + " " + e.message + " path=" + JSON.stringify(e.path));
}
try {
  req(${JSON.stringify(islandRel)});
  process._rawDebug("[child] require SUCCEEDED (unexpected for a text .node)");
} catch (e) {
  process._rawDebug("[child] REQUIRE " + e.code + " :: " + String(e.message).split("\\n")[0] + " path=" + JSON.stringify(e.path));
}
`,
);

const tracer = join(root, "trace.cjs");
writeFileSync(
  tracer,
  `const Module = require("node:module");
const path = require("node:path");
Error.stackTraceLimit = 100;
const original = Module._findPath;
Module._findPath = function (request, paths, isMain) {
  try {
    const shown = Array.isArray(paths)
      ? paths.slice(0, 3).map((p) => p + " => " + path.resolve(p, request))
      : String(paths);
    process._rawDebug("[findPath] " + JSON.stringify(request) + " isMain=" + isMain + " :: " + JSON.stringify(shown));
  } catch (e) {
    process._rawDebug("[findPath] trace error " + e.message);
  }
  return original.call(this, request, paths, isMain);
};
`,
);

// Mirrors what tests/compile-native-islands/driver.mjs hands the artifact: an
// explicit env object spread from process.env, which drops Windows' hidden
// "=C:" drive-current-directory variables.
const spreadEnv = { ...process.env };
delete spreadEnv.NODE_OPTIONS;

const verbatim = (p) => `\\\\?\\${p}`;

function run(label, { spellings, cwd, env, trace }) {
  const entry = spellings(join(app, "entry.mjs"));
  const boot = spellings(join(app, "boot.cjs"));
  const args = [];
  if (trace) args.push(`--require=${tracer}`);
  args.push(`--require=${boot}`, entry);
  const result = spawnSync(process.execPath, args, { cwd, env, encoding: "utf8" });
  console.log(`\n===== ${label} =====`);
  console.log(`entry: ${entry}`);
  console.log(`cwd:   ${cwd}`);
  console.log(`exit:  ${result.status}`);
  console.log(result.stderr.trimEnd());
  if (result.stdout.trim()) console.log("-- stdout --\n" + result.stdout.trimEnd());
}

const otherDrive = process.env.GITHUB_WORKSPACE || process.cwd();

run("A plain entry, foreign cwd, spread env", { spellings: (p) => p, cwd: foreign, env: spreadEnv });
run("B VERBATIM entry, foreign cwd, spread env", { spellings: verbatim, cwd: foreign, env: spreadEnv });
run("C VERBATIM entry, foreign cwd, inherited env", { spellings: verbatim, cwd: foreign, env: process.env });
run("D VERBATIM entry, other-drive cwd, spread env", { spellings: verbatim, cwd: otherDrive, env: spreadEnv });
run("E VERBATIM entry, foreign cwd, spread env, TRACED", {
  spellings: verbatim,
  cwd: foreign,
  env: spreadEnv,
  trace: true,
});

console.log("\n===== raw path arithmetic on this host =====");
const path = await import("node:path");
const url = await import("node:url");
const appVerbatim = verbatim(app);
console.log("resolve(verbatimApp, rel) =", path.resolve(appVerbatim, islandRel));
console.log("pathToFileURL(verbatimEntry) =", url.pathToFileURL(join(appVerbatim, "entry.mjs")).href);
console.log(
  "fileURLToPath(pathToFileURL(verbatimEntry)) =",
  url.fileURLToPath(url.pathToFileURL(join(appVerbatim, "entry.mjs"))),
);
// THE CULPRIT, isolated: Node's own realpathSync cannot walk a verbatim path.
// Reported so the probe still exits 0 and the run conclusion stays meaningful.
try {
  console.log("realpathSync(verbatimEntry) =", (await import("node:fs")).realpathSync(join(appVerbatim, "entry.mjs")));
} catch (error) {
  console.log(`realpathSync(verbatimEntry) THREW ${error.code} path=${JSON.stringify(error.path)} :: ${error.message}`);
}
console.log("PROBE_GEOMETRY_DONE");
