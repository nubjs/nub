// Run against a built compiler: node probe.mjs --nub /path/to/nub --out /tmp/cache-probe
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const args = new Map();
for (let i = 2; i < process.argv.length; i += 2) args.set(process.argv[i], process.argv[i + 1]);
const nub = path.resolve(args.get("--nub"));
const root = path.resolve(args.get("--out"));
const target = args.get("--target") ?? "26.6.0";
const expectCache = args.get("--expect-cache") !== "false";
const expectWarmReuse = args.get("--expect-warm-reuse") !== "false";
const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("NODE_") && !key.startsWith("__NUB_") && !key.startsWith("NUB_")));
for (const key of ["NVM_DIR", "FNM_DIR", "VOLTA_HOME", "ASDF_DATA_DIR", "MISE_DATA_DIR"]) delete env[key];
env.PATH = path.dirname(process.execPath) + path.delimiter + env.PATH;
if (process.env.__NUB_LAUNCHER_TEMPLATE) env.__NUB_LAUNCHER_TEMPLATE = process.env.__NUB_LAUNCHER_TEMPLATE;
fs.mkdirSync(root, { recursive: true });
const source = path.join(root, "source");
const foreign = path.join(root, "foreign");
fs.mkdirSync(foreign, { recursive: true });
function write(name, value) {
  const file = path.join(source, name);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, value);
}
write("package.json", JSON.stringify({ name: "cache-probe", version: "1.0.0", type: "module", dependencies: { "cache-fixture": "1.0.0" } }));
write("node_modules/cache-fixture/package.json", JSON.stringify({ name: "cache-fixture", version: "1.0.0", type: "module", main: "index.js" }));
write("node_modules/cache-fixture/index.js", "import fs from 'node:fs'; if(process.env.CACHE_PROBE_EFFECT) fs.writeFileSync(process.env.CACHE_PROBE_EFFECT,'executed');\n" +
  Array.from({ length: 8000 }, (_, i) => `export function f${i}(x) { return x + ${i}; }`).join("\n"));
write("asset.txt", "embedded asset");
write("unused.txt", "unused embedded asset");
write("a.mjs", "import {b} from './b.mjs'; export function a(){return 'a'}; export const pair = a()+b();");
write("b.mjs", "import {a} from './a.mjs'; export function b(){return a()+'b'};");
write("late.mjs", "await Promise.resolve(); export const answer=42;");
write("worker.mjs", "import {parentPort} from 'node:worker_threads'; parentPort.postMessage((await import('./late.mjs')).answer);");
write("main.mjs", `import fs from 'node:fs'; import {Worker} from 'node:worker_threads';
import {getCompileCacheDir} from 'node:module'; import {f7999} from 'cache-fixture'; import {pair} from './a.mjs';
const worker=new Worker(new URL('./worker.mjs',import.meta.url));
const answer=await new Promise((resolve,reject)=>{worker.on('message',resolve);worker.on('error',reject)});
console.log(JSON.stringify({value:f7999(1),pair,answer,late:(await import('./late.mjs')).answer,
asset:fs.readFileSync(new URL('./asset.txt',import.meta.url),'utf8'),preload:globalThis.cacheProbePreload??null,
url:import.meta.url,cache:getCompileCacheDir()}));`);
write("small.mjs", "console.log('small program');");

const ext = process.platform === "win32" ? ".exe" : "";
const effect = path.join(root, "build-effect");
check(spawnSync(process.execPath, [path.join(source, "main.mjs")], {
  env, encoding: "utf8", timeout: 30000,
}), "plain Node control");
console.log("PASS plain Node control");
function compile(name, entry, flags) {
  const out = path.join(root, name + ext);
  const result = spawnSync(nub, ["compile", path.join(source, entry), "--out", out, "--target", target, ...flags], {
    env: { ...env, CACHE_PROBE_EFFECT: effect }, encoding: "utf8", timeout: 180000, maxBuffer: 10 * 1024 * 1024,
  });
  fs.writeFileSync(path.join(root, `${name}-build.log`), result.stdout + result.stderr);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.equal(fs.existsSync(effect), false, "the build evaluated application code");
  return out;
}
const flags = ["--unbundled", "cache-fixture", "--include", path.join(source, "asset.txt"), "--include", path.join(source, "unused.txt")];
const binary = compile("large", "main.mjs", flags);
const smol = compile("smol", "main.mjs", ["--smol", ...flags]);
const small = compile("small", "small.mjs", []);
fs.renameSync(source, path.join(root, "hidden-source"));
const home = path.join(root, "home");
fs.mkdirSync(home, { recursive: true });
const runEnv = { ...env, HOME: home, USERPROFILE: home, XDG_CACHE_HOME: path.join(home, "cache"), NODE_DEBUG_NATIVE: "COMPILE_CACHE" };
function check(result, label, preload = null) {
  assert.equal(result.status, 0, `${label}: ${result.stdout}\n${result.stderr}`);
  const data = JSON.parse(result.stdout.trim().split("\n").pop());
  assert.deepEqual({ ...data, url: undefined, cache: undefined }, { value: 8000, pair: "aab", answer: 42, late: 42, asset: "embedded asset", preload, url: undefined, cache: undefined });
  return data;
}
function run(label, extra = {}, executable = binary, preload = null) {
  const result = spawnSync(executable, [], { env: { ...runEnv, ...extra }, cwd: foreign, encoding: "utf8", timeout: 30000 });
  fs.writeFileSync(path.join(root, `${label}-run.log`), result.stdout + result.stderr);
  const data = check(result, label, preload);
  console.log(`PASS ${label}`);
  return { result, data };
}
const cold = run("cold");
const seeded = fs.readdirSync(cold.data.cache).some((name) => name.startsWith(".nub-"));
assert.equal(seeded, expectCache, "pack installation did not match the compiler under test");
if (expectCache) assert.match(cold.result.stderr, /V8 code cache for ESM .*cache-fixture\/index\.js was accepted/);
run("warm");

let appDir = path.dirname(fileURLToPath(cold.data.url));
while (!fs.existsSync(path.join(appDir, ".nub-complete"))) {
  const parent = path.dirname(appDir);
  assert.notEqual(parent, appDir, "the compiled entry must belong to a published extraction");
  appDir = parent;
}
const marker = path.join(appDir, ".nub-complete");
function filesUnder(dir) {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const file = path.join(dir, entry.name);
    return entry.isDirectory() ? filesUnder(file) : [file];
  });
}
const unused = filesUnder(appDir).find((file) => path.basename(file) === "unused.txt");
assert.ok(unused, "the unused included asset must be extracted");
const extraFile = path.join(appDir, "runtime-generated.txt");
fs.writeFileSync(extraFile, "not in the payload");
fs.rmSync(unused);
run("published-cache-reuse");
assert.equal(fs.existsSync(extraFile), expectWarmReuse, "warm reuse rescanned or replaced the published tree");
assert.equal(fs.existsSync(unused), !expectWarmReuse, "warm reuse repaired an unrelated missing file");

fs.rmSync(marker);
run("missing-marker-repair");
assert.equal(fs.readFileSync(unused, "utf8"), "unused embedded asset");
assert.equal(fs.existsSync(extraFile), false);
fs.writeFileSync(marker, "incomplete");
run("invalid-marker-repair");
assert.equal(fs.statSync(marker).size, 0);

// Publication must remain recoverable even when several processes extract at once.
fs.rmSync(appDir, { recursive: true });
await Promise.all(Array.from({ length: 4 }, (_, i) => new Promise((resolve, reject) => {
  const child = spawn(binary, [], { env: runEnv, cwd: foreign });
  let stdout = "", stderr = "";
  child.stdout.on("data", (data) => { stdout += data; });
  child.stderr.on("data", (data) => { stderr += data; });
  child.on("error", reject);
  child.on("close", (status) => {
    try { check({ status, stdout, stderr }, `concurrent-extraction-${i}`); resolve(); } catch (error) { reject(error); }
  });
})));
assert.equal(fs.readFileSync(unused, "utf8"), "unused embedded asset");
assert.equal(fs.statSync(marker).size, 0);
console.log("PASS concurrent extraction publication");

const bootstrap = path.join(appDir, "__nub_compile_bootstrap.cjs");
fs.rmSync(bootstrap);
run("missing-bootstrap-repair");
fs.rmSync(bootstrap);
fs.mkdirSync(bootstrap);
run("invalid-bootstrap-repair");
assert.ok(fs.lstatSync(bootstrap).isFile());
if (process.platform !== "win32") {
  const replacement = path.join(root, "replacement-bootstrap.cjs");
  fs.writeFileSync(replacement, "throw new Error('replaced bootstrap executed')");
  fs.rmSync(bootstrap);
  fs.symlinkSync(replacement, bootstrap);
  run("symlinked-bootstrap-repair");
  assert.ok(fs.lstatSync(bootstrap).isFile());
}

if (process.platform !== "win32") {
  const dirs = [appDir, path.dirname(appDir)];
  const modes = dirs.map((dir) => fs.statSync(dir).mode & 0o777);
  try {
    for (const dir of dirs) fs.chmodSync(dir, 0o500);
    run("read-only-published-app");
  } finally {
    dirs.forEach((dir, i) => fs.chmodSync(dir, modes[i]));
  }
}
run("disabled", { NODE_DISABLE_COMPILE_CACHE: "1" });
run("portable", { NODE_COMPILE_CACHE: path.join(root, "portable"), NODE_COMPILE_CACHE_PORTABLE: "1" });
const readOnly = path.join(root, "read-only");
const readOnlyTagged = path.join(readOnly, path.basename(cold.data.cache));
fs.mkdirSync(readOnlyTagged, { recursive: true });
run("read-only", { NODE_COMPILE_CACHE: readOnly, NODE_COMPILE_CACHE_READONLY: "1" });
assert.equal(fs.readdirSync(readOnlyTagged).some((name) => name.startsWith(".nub-")), false);
const [targetMajor, targetMinor] = target.split(".").map(Number);
if (targetMajor > 26 || (targetMajor === 26 && targetMinor >= 8)) {
  assert.deepEqual(fs.readdirSync(readOnlyTagged), [], "read-only execution wrote cache files");
}
run("different-flags", { NODE_OPTIONS: "--jitless" });
fs.writeFileSync(path.join(root, "not-a-directory"), "cache unavailable");
run("unwritable-cache", { NODE_COMPILE_CACHE: path.join(root, "not-a-directory") });
const preload = path.join(root, "preload.cjs");
fs.writeFileSync(preload, "globalThis.cacheProbePreload='present'");
run("preloaded", { NODE_OPTIONS: `--require=${JSON.stringify(preload)}` }, binary, "present");
const shared = path.join(root, "concurrent-cache");
await Promise.all(Array.from({ length: 4 }, (_, i) => new Promise((resolve, reject) => {
  const child = spawn(binary, [], { env: { ...runEnv, NODE_COMPILE_CACHE: shared }, cwd: foreign });
  let stdout = "", stderr = "";
  child.stdout.on("data", (data) => { stdout += data; });
  child.stderr.on("data", (data) => { stderr += data; });
  child.on("error", reject);
  child.on("close", (status) => {
    try { check({ status, stdout, stderr }, `concurrent-${i}`); resolve(); } catch (error) { reject(error); }
  });
})));
console.log("PASS concurrent first cache publication");
run("smol", {}, smol);
const smallResult = spawnSync(small, [], { env: runEnv, cwd: foreign, encoding: "utf8", timeout: 30000 });
assert.equal(smallResult.status, 0, smallResult.stderr);
assert.equal(smallResult.stdout.trim(), "small program");
console.log("PASS small program");
assert.equal(fs.existsSync(source), false, "sources must stay hidden throughout execution");
assert.ok(!fileURLToPath(cold.data.url).startsWith(source));
console.log("COMPILE_CACHE_PROBE_OK");
