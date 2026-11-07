import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import test from "node:test";

const runtime = fileURLToPath(new URL("../runtime/", import.meta.url));
const require = createRequire(import.meta.url);
const moduleUrl = (name: string): string => JSON.stringify(pathToFileURL(join(runtime, name)).href);
function child(source: string): void {
  const result = spawnSync(process.execPath, ["--input-type=module", "-e", `
    import assert from 'node:assert/strict';
    import { createRequire } from 'node:module';
    const require = createRequire(${JSON.stringify(import.meta.url)});
    ${source}
  `], { encoding: "utf8", timeout: 10_000 });
  assert.ifError(result.error);
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
}

void test("navigator leaves modern lazy globals untouched", () => child(`
  Object.defineProperty(globalThis, 'navigator', { configurable: true, get() { throw Error('read native'); } });
  const getter = Object.getOwnPropertyDescriptor(globalThis, 'navigator').get;
  const { installNavigatorShim } = await import(${moduleUrl("navigator-shim.mjs")});
  installNavigatorShim();
  assert.equal(Object.getOwnPropertyDescriptor(globalThis, 'navigator').get, getter);
`));

void test("navigator floor bootstrap supplies locale, hardware and stable native-shaped globals", () => child(`
  Object.defineProperty(process.versions, 'node', { value: '18.19.0' });
  Object.defineProperty(process, 'getBuiltinModule', { value: undefined, configurable: true });
  delete globalThis.navigator; delete globalThis.Navigator;
  process.env.LC_ALL = 'fr_CA.UTF-8';
  const shim = await import(${moduleUrl("navigator-shim.mjs")});
  shim.setBootstrapCreateRequire(createRequire);
  shim.installNavigatorShim();
  const original = navigator;
  assert.equal(navigator.userAgent, 'Node.js/18');
  assert.equal(navigator.language, 'fr-CA');
  assert.deepEqual(navigator.languages, ['fr-CA']);
  assert.equal(Object.isFrozen(navigator.languages), true);
  assert.equal(navigator.hardwareConcurrency, require('node:os').availableParallelism());
  assert.ok(navigator instanceof Navigator);
  assert.equal(Object.getOwnPropertyDescriptor(globalThis, 'navigator').enumerable, false);
  shim.installNavigatorShim(); assert.equal(navigator, original);
`));

const locksSetup = `
  Object.defineProperty(globalThis, 'navigator', { value: {}, configurable: true });
  const { installNavigatorLocks } = await import(${moduleUrl("navigator-locks.mjs")});
  const locks = navigator.locks;
  installNavigatorLocks(); assert.equal(navigator.locks, locks);
`;
void test("Web Locks serializes writers without letting shared readers bypass the queue", () => child(locksSetup + `
  let release;
  const held = new Promise(resolve => { release = resolve; });
  const order = [];
  const first = locks.request('item', { mode: 'shared' }, async lock => {
    assert.equal(lock.name, 'item'); assert.equal(lock.mode, 'shared');
    order.push('first'); await held; return 42;
  });
  const writer = locks.request('item', () => { order.push('writer'); });
  const reader = locks.request('item', { mode: 'shared' }, () => { order.push('reader'); });
  await Promise.resolve();
  const state = await locks.query();
  assert.deepEqual(state.held.map(x => x.mode), ['shared']);
  assert.deepEqual(state.pending.map(x => x.mode), ['exclusive', 'shared']);
  assert.equal(await locks.request('item', { ifAvailable: true }, lock => lock), null);
  release(); assert.equal(await first, 42); await Promise.all([writer, reader]);
  assert.deepEqual(order, ['first', 'writer', 'reader']);
  assert.deepEqual(await locks.query(), { held: [], pending: [] });
`));

void test("Web Locks handles abort, stealing, callback errors and invalid options", () => child(locksSetup + `
  const reason = new Error('abort'); const controller = new AbortController(); controller.abort(reason);
  await assert.rejects(locks.request('x', { signal: controller.signal }, () => assert.fail('aborted callback')), e => e === reason);
  await assert.rejects(locks.request('-reserved', () => {}), { name: 'NotSupportedError' });
  await assert.rejects(locks.request('x', { mode: 'bad' }, () => {}), TypeError);
  await assert.rejects(locks.request('x', { steal: true, ifAvailable: true }, () => {}), { name: 'NotSupportedError' });
  const error = new Error('callback');
  await assert.rejects(locks.request('x', () => { throw error; }), e => e === error);
  assert.equal(await locks.request('x', () => 1), 1);
  let release; const waiting = new Promise(resolve => { release = resolve; });
  const stolen = locks.request('x', () => waiting);
  const rejected = assert.rejects(stolen, { name: 'AbortError' });
  assert.equal(await locks.request('x', { steal: true }, () => 2), 2);
  await rejected; release();
`));

void test("Web Locks preserves a preexisting implementation", () => child(`
  const sentinel = {};
  Object.defineProperty(globalThis, 'navigator', { value: { locks: sentinel }, configurable: true });
  const { installNavigatorLocks } = await import(${moduleUrl("navigator-locks.mjs")});
  installNavigatorLocks(); assert.equal(navigator.locks, sentinel);
`));

void test("Worker shim preserves native Worker and avoids eager error-event realization", () => child(`
  class NativeWorker {}
  globalThis.Worker = NativeWorker;
  Object.defineProperty(globalThis, 'ErrorEvent', { configurable: true, get() { throw Error('eager ErrorEvent'); } });
  const { installWorkerPolyfill } = await import(${moduleUrl("worker-polyfill.mjs")});
  installWorkerPolyfill(); assert.equal(Worker, NativeWorker);
`));

void test("Worker shim sends real messages, transfers buffers and terminates", () => child(`
  delete globalThis.Worker;
  await import(${moduleUrl("worker-polyfill.mjs")});
  const worker = new Worker("import('node:worker_threads').then(({parentPort}) => parentPort.on('message', data => parentPort.postMessage(data, [data.buffer])));", { eval: true, execArgv: [] });
  try {
    const data = new Uint8Array([3, 5, 8]);
    const received = new Promise((resolve, reject) => {
      worker.addEventListener('message', event => resolve(event.data), { once: true });
      worker.addEventListener('error', event => reject(event.error), { once: true });
    });
    worker.postMessage(data, [data.buffer]);
    assert.equal(data.byteLength, 0);
    assert.deepEqual(Array.from(await received), [3, 5, 8]);
  } finally { assert.equal(typeof await worker.terminate(), 'number'); }
`));

void test("Worker shim delivers worker errors with their original details", () => child(`
  delete globalThis.Worker; delete globalThis.ErrorEvent;
  await import(${moduleUrl("worker-polyfill.mjs")});
  const worker = new Worker("throw new Error('worker unit failure')", { eval: true, execArgv: [] });
  const error = await new Promise(resolve => worker.addEventListener('error', event => {
    event.preventDefault(); resolve(event);
  }, { once: true }));
  assert.match(error.message, /worker unit failure/);
  assert.equal(error.error.message, 'worker unit failure');
  await worker.terminate();
`));

void test("Worker blob sources install the worker-side globals and reject revoked URLs", () => child(`
  delete globalThis.Worker;
  await import(${moduleUrl("worker-polyfill.mjs")});
  const source = "import " + JSON.stringify(${moduleUrl("worker-polyfill.mjs")}) + "; postMessage({ name: self.name, same: self === globalThis, enumerable: Object.keys(globalThis).includes('postMessage') }); close();";
  const url = URL.createObjectURL(new Blob([source], { type: 'text/javascript' }));
  const worker = new Worker(url, { name: 'shim-unit' });
  URL.revokeObjectURL(url);
  try {
    const data = await new Promise((resolve, reject) => {
      worker.addEventListener('message', event => resolve(event.data), { once: true });
      worker.addEventListener('error', event => reject(event.error), { once: true });
    });
    assert.deepEqual(data, { name: 'shim-unit', same: true, enumerable: false });
    assert.throws(() => new Worker(url), TypeError);
    assert.throws(() => new Worker({}), TypeError);
  } finally { await worker.terminate(); }
`));

function packageRoot(name: string): string {
  let path = dirname(require.resolve(name));
  for (;;) {
    try { if (JSON.parse(readFileSync(join(path, 'package.json'), 'utf8')).name === name) return path; } catch {}
    const parent = dirname(path);
    if (parent === path) throw new Error(`Cannot locate package root for ${name}`);
    path = parent;
  }
}
// Staging the runtime source files makes package resolution explicit for
// the ESM Temporal wrapper as well as the CommonJS lazy installers.
function lazyChild(source: string): void {
  const dir = mkdtempSync(join(tmpdir(), 'nub-lazy-unit-'));
  try {
    for (const name of ['primordials.cjs', 'compile-lazy.cjs', 'compile-lazy-temporal.cjs', 'compile-lazy-temporal.mjs', 'compile-lazy-urlpattern.cjs', 'compile-lazy-float16.cjs', 'preload-common.cjs', 'pnp-util.cjs']) {
      copyFileSync(join(runtime, name), join(dir, name));
    }
    for (const name of ['@js-temporal/polyfill', '@petamoriken/float16', 'urlpattern-polyfill']) {
      const dest = join(dir, 'node_modules', name); mkdirSync(dirname(dest), { recursive: true });
      symlinkSync(packageRoot(name), dest, 'junction');
    }
    child(`const staged = createRequire(${JSON.stringify(join(dir, 'entry.cjs'))}); ${source}`);
  } finally { rmSync(dir, { recursive: true, force: true }); }
}

void test("lazy properties initialize once, support assignment and retry failed loads", () => {
  const { defineLazy } = require('../runtime/compile-lazy.cjs');
  const object: { value?: number; override?: number; retry?: number } = {}; let calls = 0;
  defineLazy(object, 'value', () => ++calls, false);
  assert.equal(calls, 0); assert.equal(object.value, 1); assert.equal(object.value, 1);
  assert.deepEqual(Object.getOwnPropertyDescriptor(object, 'value'), { value: 1, writable: true, configurable: true, enumerable: false });
  defineLazy(object, 'override', () => assert.fail('assignment must not load'));
  object.override = 7; assert.equal(object.override, 7);
  defineLazy(object, 'retry', () => { if (++calls === 2) throw Error('retry'); return 3; });
  assert.throws(() => object.retry, /retry/); assert.equal(object.retry, 3);
});

void test("compiled lazy polyfills expose real APIs and settle accessor descriptors", () => lazyChild(`
  delete globalThis.Temporal; delete globalThis.URLPattern; delete globalThis.Float16Array;
  delete DataView.prototype.getFloat16; delete DataView.prototype.setFloat16; delete Math.f16round;
  staged('./compile-lazy-temporal.cjs').installCompiledTemporalLazyGlobal();
  staged('./compile-lazy-urlpattern.cjs').installCompiledUrlPatternLazyGlobal();
  staged('./compile-lazy-float16.cjs').installCompiledFloat16LazyGlobals();
  for (const name of ['Temporal','URLPattern','Float16Array']) assert.equal(typeof Object.getOwnPropertyDescriptor(globalThis,name).get, 'function');
  assert.equal(Temporal.PlainDate.from('2024-02-28').add({days: 1}).toString(), '2024-02-29');
  assert.equal(new URLPattern({pathname: '/users/:id'}).exec('https://example.com/users/42').pathname.groups.id, '42');
  assert.deepEqual(Array.from(new Float16Array([1.5,-2])), [1.5,-2]);
  const view = new DataView(new ArrayBuffer(2)); view.setFloat16(0, 1.5, true); assert.equal(view.getFloat16(0,true),1.5);
  assert.equal(Math.f16round(1.5), 1.5);
  assert.equal(Object.getOwnPropertyDescriptor(globalThis,'Temporal').enumerable,false);
  for (const name of ['Temporal','URLPattern','Float16Array']) assert.equal(Object.getOwnPropertyDescriptor(globalThis,name).get,undefined);
`));

void test("compiled lazy installers preserve supplied globals and pre-access overrides", () => lazyChild(`
  for (const [name,file,method] of [
    ['Temporal','temporal','installCompiledTemporalLazyGlobal'],
    ['URLPattern','urlpattern','installCompiledUrlPatternLazyGlobal'],
    ['Float16Array','float16','installCompiledFloat16LazyGlobals'],
  ]) {
    const install = staged('./compile-lazy-'+file+'.cjs')[method];
    const native = {}; globalThis[name] = native; install(); assert.equal(globalThis[name], native);
    delete globalThis[name]; install(); const replacement = {}; globalThis[name] = replacement;
    assert.equal(globalThis[name], replacement);
  }
  for (const [present, missing] of [['getFloat16', 'setFloat16'], ['setFloat16', 'getFloat16']]) {
    delete globalThis.Float16Array;
    delete DataView.prototype[missing];
    const native = function () { return 'native'; };
    Object.defineProperty(DataView.prototype, present, {value: native, writable: true, configurable: true});
    staged('./compile-lazy-float16.cjs').installCompiledFloat16LazyGlobals();
    assert.equal(DataView.prototype[present], native);
    assert.equal(typeof Object.getOwnPropertyDescriptor(DataView.prototype, missing).get, 'function');
    assert.equal(typeof DataView.prototype[missing], 'function');
    assert.equal(DataView.prototype[present], native);
  }
`));

void test("floor builtin bootstrap supplies createRequire without exposing globals", () => child(`
  const before = new Set(Reflect.ownKeys(globalThis));
  const floor = await import(${moduleUrl("floor-builtin.mjs")});
  assert.equal(floor.createRequire, createRequire);
  assert.equal(floor.createRequire(${JSON.stringify(import.meta.url)})('node:path'), require('node:path'));
  assert.deepEqual(Reflect.ownKeys(globalThis).filter(key => !before.has(key)), []);
`));
