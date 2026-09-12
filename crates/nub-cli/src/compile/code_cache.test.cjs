const assert = require("node:assert/strict");
const { spawn, spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { pathToFileURL } = require("node:url");
const { test } = require("node:test");
const { createHash } = require("node:crypto");
const { zstdCompressSync, zstdDecompressSync, constants } = require("node:zlib");

const supported = Number(process.versions.node.split(".")[0]) >= 24;
const [major, minor] = process.versions.node.split(".").map(Number);
const readOnlySupported = major > 26 || (major === 26 && minor >= 8);
const generator = path.join(__dirname, "code_cache_generate.cjs");
const installer = fs.readFileSync(path.join(__dirname, "code_cache_install.cjs"), "utf8");
const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("NODE_") && !key.startsWith("__NUB_")));
const compress = (pack) => zstdCompressSync(pack, { params: { [constants.ZSTD_c_compressionLevel]: 9 } });

function fixture(t, additional = []) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "nub-code-cache-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const app = path.join(root, "moved app # % λ");
  const cache = path.join(root, "cache");
  fs.mkdirSync(app);
  const effect = path.join(root, "executed");
  const source = `import fs from 'node:fs'; fs.writeFileSync(${JSON.stringify(effect)}, 'yes');\n` +
    Array.from({ length: 2000 }, (_, i) => `export function f${i}(x){return x+${i}}`).join("\n");
  const files = [
    ["nested #/λ%.mjs", source],
    ["main.mjs", "import {f1999} from './nested%20%23/%CE%BB%25.mjs'; await Promise.resolve(); console.log(f1999(1),import.meta.url);"],
    ...additional,
  ];
  const build = spawnSync(process.execPath, ["--predictable", "--experimental-vm-modules", generator], {
    input: JSON.stringify(files), env, maxBuffer: 10 * 1024 * 1024,
  });
  assert.equal(build.status, 0, build.stderr.toString());
  assert.equal(fs.existsSync(effect), false, "generating bytecode must not evaluate top-level code");
  const pack = build.stdout;
  const index = JSON.parse(pack.subarray(4, 4 + pack.readUInt32LE(0)));
  const id = createHash("sha256").update(pack).digest("hex");
  fs.writeFileSync(path.join(app, "pack.bin"), compress(pack));
  for (const [name, code] of files) {
    fs.mkdirSync(path.dirname(path.join(app, name)), { recursive: true });
    fs.writeFileSync(path.join(app, name), code);
  }
  const bootstrap = path.join(app, "bootstrap.cjs");
  fs.writeFileSync(bootstrap,
    `process[Symbol.for('nub.compile.bootstrap')]={getBuiltin:process.getBuiltinModule.bind(process)};\n` +
    installer + `(...${JSON.stringify(["pack.bin", id, index.version, index.arch, index.tag])});`);
  function run(extra = {}, flags = []) {
    const result = spawnSync(process.execPath, ["--require", bootstrap, ...flags, path.join(app, "main.mjs")], {
      env: { ...env, NODE_COMPILE_CACHE: cache, NODE_DEBUG_NATIVE: "COMPILE_CACHE", ...extra },
    });
    assert.equal(result.status, 0, result.stderr.toString());
    return result;
  }
  return { root, app, cache, effect, id, run, bootstrap };
}

test("eager bytecode is accepted at relocated Unicode URLs without evaluating during build", { skip: !supported }, (t) => {
  const f = fixture(t);
  const first = f.run();
  assert.equal(first.stdout.toString().trim(), `2000 ${pathToFileURL(fs.realpathSync(path.join(f.app, "main.mjs"))).href}`);
  assert.equal(fs.readFileSync(f.effect, "utf8"), "yes");
  assert.match(first.stderr.toString(), /V8 code cache for ESM .*main\.mjs was accepted/);
  assert.match(first.stderr.toString(), /V8 code cache for ESM .*%CE%BB%25\.mjs was accepted/);
  const directories = fs.readdirSync(f.cache);
  const directory = path.join(f.cache, directories[0]);
  const marker = path.join(directory, fs.readdirSync(directory).find((name) => name.startsWith(`.nub-${f.id}-`)));
  const time = fs.statSync(marker).mtimeMs;
  const second = f.run();
  assert.equal(second.stdout.toString(), first.stdout.toString());
  assert.equal(fs.statSync(marker).mtimeMs, time, "warm starts must not reseed");
});

test("source changes use Node's normal source-validation fallback", { skip: !supported }, (t) => {
  const f = fixture(t);
  f.run();
  const name = path.join(f.app, "nested #/λ%.mjs");
  fs.writeFileSync(name, fs.readFileSync(name, "utf8").replace("return x+1999", "return x+1998"));
  const result = f.run();
  assert.match(result.stdout.toString(), /^1999 /);
  assert.match(result.stderr.toString(), /code hash mismatch/);
});

for (const [name, extra, flags] of [
  ["disabled", { NODE_DISABLE_COMPILE_CACHE: "1" }, []],
  ["portable", { NODE_COMPILE_CACHE_PORTABLE: "1" }, []],
  ["read-only", { NODE_COMPILE_CACHE_READONLY: "1" }, []],
  ["different V8 flags", {}, ["--no-lazy"]],
]) {
  test(`${name} skips packaged caches`, { skip: !supported }, (t) => {
    const f = fixture(t);
    assert.match(f.run(extra, flags).stdout.toString(), /^2000 /);
    const all = fs.existsSync(f.cache) ? fs.readdirSync(f.cache, { recursive: true }) : [];
    assert.equal(all.some((name) => name.includes(`.nub-${f.id}`)), false);
  });
}

test("read-only caches remain unchanged even when their directory is writable", { skip: !readOnlySupported }, (t) => {
  const f = fixture(t);
  const prepare = spawnSync(process.execPath, ["-p", "require('node:module').getCompileCacheDir()"], {
    env: { ...env, NODE_COMPILE_CACHE: f.cache }, encoding: "utf8",
  });
  assert.equal(prepare.status, 0, prepare.stderr);
  const directory = prepare.stdout.trim();
  assert.ok(fs.statSync(directory).isDirectory());
  const sentinel = path.join(directory, "sentinel");
  fs.writeFileSync(sentinel, "existing cache");
  const snapshot = () => fs.readdirSync(directory).sort().map((name) => {
    const file = path.join(directory, name);
    return [name, createHash("sha256").update(fs.readFileSync(file)).digest("hex"), fs.statSync(file).mtimeMs];
  });
  const before = snapshot();
  const directoryTime = fs.statSync(directory).mtimeMs;
  assert.match(f.run({ NODE_COMPILE_CACHE_READONLY: "1" }).stdout.toString(), /^2000 /);
  assert.deepEqual(snapshot(), before);
  assert.equal(fs.statSync(directory).mtimeMs, directoryTime);
});

test("a missing cache pack leaves program execution intact", { skip: !supported }, (t) => {
  const f = fixture(t);
  fs.unlinkSync(path.join(f.app, "pack.bin"));
  assert.match(f.run().stdout.toString(), /^2000 /);
});

test("corrupt cached data falls back to source", { skip: !supported }, (t) => {
  const f = fixture(t);
  const file = path.join(f.app, "pack.bin");
  const pack = zstdDecompressSync(fs.readFileSync(file));
  pack[pack.length - 1] ^= 0xff;
  fs.writeFileSync(file, compress(pack));
  const result = f.run();
  assert.match(result.stdout.toString(), /^2000 /);
  assert.match(result.stderr.toString(), /hash mismatch/);
});

test("a truncated compressed pack leaves program execution intact", { skip: !supported }, (t) => {
  const f = fixture(t);
  const file = path.join(f.app, "pack.bin");
  fs.writeFileSync(file, fs.readFileSync(file).subarray(0, 12));
  assert.match(f.run().stdout.toString(), /^2000 /);
});

test("a shared cache seeds each relocated application", { skip: !supported }, (t) => {
  const f = fixture(t);
  f.run();
  const moved = path.join(f.root, "another location");
  fs.cpSync(f.app, moved, { recursive: true });
  const result = spawnSync(process.execPath, ["--require", path.join(moved, "bootstrap.cjs"), path.join(moved, "main.mjs")], {
    env: { ...env, NODE_COMPILE_CACHE: f.cache, NODE_DEBUG_NATIVE: "COMPILE_CACHE" },
  });
  assert.equal(result.status, 0, result.stderr.toString());
  assert.match(result.stderr.toString(), /V8 code cache for ESM .*main\.mjs was accepted/);
});

test("concurrent first starts publish complete cache entries", { skip: !supported }, async (t) => {
  const f = fixture(t);
  await Promise.all(Array.from({ length: 4 }, () => new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ["--require", f.bootstrap, path.join(f.app, "main.mjs")], {
      env: { ...env, NODE_COMPILE_CACHE: f.cache },
    });
    let stdout = "", stderr = "";
    child.stdout.on("data", (data) => { stdout += data; });
    child.stderr.on("data", (data) => { stderr += data; });
    child.on("error", reject);
    child.on("close", (code) => {
      try {
        assert.equal(code, 0, stderr);
        assert.match(stdout, /^2000 /);
        resolve();
      } catch (error) { reject(error); }
    });
  })));
  assert.match(f.run().stderr.toString(), /V8 code cache for ESM .*main\.mjs was accepted/);
  assert.equal(fs.readdirSync(f.cache, { recursive: true }).some((name) => name.includes(".nub-cache-")), false);
});

test("cached modules preserve dynamic imports, cycles, workers and source locations", { skip: !supported }, (t) => {
  const f = fixture(t, [
    ["a.mjs", "import {b} from './b.mjs'; export function a(){return 'a'}; export const pair = a()+b();"],
    ["b.mjs", "import {a} from './a.mjs'; export function b(){return a()+'b'};"],
    ["worker.mjs", "import {parentPort} from 'node:worker_threads'; parentPort.postMessage((await import('./a.mjs')).pair);"],
    ["dynamic.mjs", "export class C { #n=41; value(){return eval('this.#n')+1} }; export const url=import.meta.url; export function fail(){throw new Error('location')}"],
    ["main.mjs", `import assert from 'node:assert/strict'; import {Worker} from 'node:worker_threads';
      const d=await import('./dynamic.mjs'); assert.equal(new d.C().value(),42);
      assert.equal((await import('./a.mjs')).pair,'aab');
      assert.equal(d.url,new URL('./dynamic.mjs',import.meta.url).href);
      assert.throws(d.fail, error => error.stack.includes(new URL('./dynamic.mjs',import.meta.url).href));
      const worker = new Worker(new URL('./worker.mjs',import.meta.url));
      assert.equal(await new Promise((resolve,reject)=>{worker.on('message',resolve);worker.on('error',reject)}),'aab');
      console.log('semantics preserved');`],
  ]);
  const result = f.run();
  assert.equal(result.stdout.toString().trim(), "semantics preserved");
  assert.match(result.stderr.toString(), /V8 code cache for ESM .*dynamic\.mjs was accepted/);
});

test("identical sources produce identical packs in separate build processes", { skip: !supported }, () => {
  const source = Array.from({ length: 12000 }, (_, i) => `export function f${i}(x){return x+${i}}`).join("\n");
  const input = JSON.stringify([["main.mjs", source]]);
  const builds = Array.from({ length: 3 }, () => {
    const result = spawnSync(process.execPath, ["--predictable", "--experimental-vm-modules", generator], {
      input, env, maxBuffer: 20 * 1024 * 1024,
    });
    assert.equal(result.status, 0, result.stderr.toString());
    return result.stdout;
  });
  assert.deepEqual(builds[0], builds[1]);
  assert.deepEqual(builds[1], builds[2]);
});
