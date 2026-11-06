import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import test from "node:test";

const file = fileURLToPath(new URL("../runtime/polyfills.cjs", import.meta.url));
const require = createRequire(file);
const source = readFileSync(file, "utf8");
const snapshot = readFileSync(require.resolve("./primordials.cjs"), "utf8");

function realm(force: string = "") {
  const context = vm.createContext({ process, Buffer, URL, URLSearchParams, TextDecoder, TextEncoder, queueMicrotask, setTimeout, clearTimeout });
  const captured: typeof import("../runtime/primordials.cjs") = vm.runInContext(`(function() { const module = { exports: {} }; ${snapshot}\nreturn module.exports; })()`, context);
  context.runtimeRequire = (id: string): unknown => id === "./primordials.cjs" ? captured : require(id);
  context.runtimeFile = file;
  vm.runInContext(`
    ${force}
    delete Math.sumPrecise;
    delete Intl.Locale.prototype.variants;
    delete Promise.try;
    if (globalThis.Iterator) { delete Iterator.zip; delete Iterator.zipKeyed; }
    const module = { exports: {} };
    (function(require, module, __filename) { ${source}\n})(runtimeRequire, module, runtimeFile);
    module.exports.installSyncPolyfills();
  `, context);
  return { context, captured, run: <T = unknown,>(code: string): T => vm.runInContext(code, context) };
}

void test("primordials are one frozen null-prototype snapshot without freezing globals", () => {
  const { captured, run } = realm();
  assert.equal(Object.getPrototypeOf(captured), null);
  assert.equal(Object.isFrozen(captured), true);
  assert.equal(run("Object.isFrozen(Object.prototype)"), false);
  assert.equal(run("Object.isFrozen(Array.prototype)"), false);
  assert.notEqual(captured.TypeError, TypeError);
  assert.equal(captured.ArrayPrototypeJoin([1, 2], ":"), "1:2");
});

void test("precise sums and locale variants survive poisoned implementation primitives", () => {
  const { run } = realm();
  const result = run<unknown[]>(`
    const poison = () => { throw new Error("poisoned internal operation"); };
    const locale = new Intl.Locale("sl-rozaj-biske");
    Math.max = poison; Object.is = poison; Number.isNaN = poison;
    DataView.prototype.setFloat64 = poison; DataView.prototype.getUint32 = poison;
    BigInt.prototype.toString = poison;
    String.prototype.split = poison; RegExp.prototype.test = poison; RegExp.prototype.exec = poison;
    Reflect.apply = poison; Function.prototype.call = poison; Function.prototype.bind = poison;
    globalThis.DataView = poison; globalThis.ArrayBuffer = poison;
    globalThis.Number = poison; globalThis.BigInt = poison;
    [Math.sumPrecise([1, 1e100, 1, -1e100]), locale.variants];
  `);
  assert.equal(result[0], 2);
  assert.equal(result[1], "biske-rozaj");
});

void test("Locale construction survives poisoned array iteration and lowercase methods", () => {
  const { run } = realm();
  const result = run<string[]>(`
    const poison = () => { throw new Error("poisoned internal operation"); };
    Array.prototype[Symbol.iterator] = poison;
    String.prototype.toLowerCase = () => "biske";
    [
      new Intl.Locale("sl").toString(),
      new Intl.Locale("sl", { region: "IT" }).toString(),
      new Intl.Locale("sl-rozaj", { variants: "rozaj" }).toString(),
    ];
  `);
  assert.equal(result[0], "sl");
  assert.equal(result[1], "sl-IT");
  assert.equal(result[2], "sl-rozaj");
});

void test("joint iterator state and private arrays survive poisoning while user reads remain observable", () => {
  const { run } = realm();
  const result = run<unknown[]>(`
    let nextReads = 0, closes = 0, steps = 0;
    const input = {
      get next() { nextReads++; return function() { return { value: ++steps, done: false }; }; },
      get return() { closes++; return function() { return {}; }; }
    };
    const poison = () => { throw new Error("poisoned internal operation"); };
    WeakMap.prototype.get = poison; WeakMap.prototype.set = poison;
    Array.prototype.push = poison; Array.prototype.every = poison;
    Array.prototype[Symbol.iterator] = poison;
    Object.create = poison; Object.defineProperty = poison;
    Reflect.apply = poison; Function.prototype.call = poison; Function.prototype.bind = poison;
    const joined = Iterator.zipKeyed({ first: input });
    const first = joined.next().value.first;
    joined.return();
    [first, nextReads, closes];
  `);
  assert.equal(result[0], 1);
  assert.equal(result[1], 1);
  assert.equal(result[2], 1);
});

void test("caller callbacks may override call without changing invocation semantics", async () => {
  const { run } = realm();
  const result = await run<Promise<unknown[]>>(`
    delete Array.fromAsync;
    module.exports.installSyncPolyfills();
    function mapper(value) { return value + this.offset; }
    mapper.call = () => { throw new Error("must not read callback.call"); };
    Array.fromAsync([1, 2], mapper, { offset: 4 });
  `);
  assert.equal(result[0], 5);
  assert.equal(result[1], 6);
});

void test("map upsert, grouping and array copies use captured implementation methods", () => {
  const { run } = realm(`
    delete Map.prototype.getOrInsert; delete Map.prototype.getOrInsertComputed;
    delete WeakMap.prototype.getOrInsert; delete WeakMap.prototype.getOrInsertComputed;
    delete Map.groupBy; delete Array.prototype.toSorted;
  `);
  const result = run<unknown[]>(`
    const map = new Map(), weak = new WeakMap(), key = {};
    const originalGet = Map.prototype.get;
    const poison = () => { throw new Error("poisoned method"); };
    Map.prototype.has = poison; Map.prototype.get = poison; Map.prototype.set = poison;
    WeakMap.prototype.has = poison; WeakMap.prototype.get = poison; WeakMap.prototype.set = poison;
    Array.from = poison; Array.prototype.sort = poison; Array.prototype.reverse = poison;
    Array.prototype.splice = poison; Array.prototype.push = poison;
    const a = map.getOrInsert("x", 3);
    const b = map.getOrInsertComputed("x", poison);
    const c = weak.getOrInsertComputed(key, () => 7);
    const grouped = Map.groupBy([1, 2, 3], n => n % 2);
    const group = Reflect.apply(originalGet, grouped, [1]);
    [a, b, c, group.length, [3, 1, 2].toSorted()[0], [1, 2].toReversed()[0], [1, 2, 3].toSpliced(1)[0]];
  `);
  assert.deepEqual(Array.from(result), [3, 3, 7, 2, 1, 2, 1]);
});

void test("base64 and hex keep internal conversion and writes stable after capture", () => {
  const { run } = realm("delete Uint8Array.prototype.toBase64;");
  const result = run<unknown[]>(`
    const bytes = new Uint8Array([0, 255]), into = new Uint8Array(3);
    const U8 = Uint8Array, Syntax = SyntaxError;
    const poison = () => { throw new Error("poisoned method"); };
    String.prototype.slice = poison; String.prototype.padStart = poison;
    String.prototype.charCodeAt = poison; String.prototype.includes = poison;
    Number.prototype.toString = poison; RegExp.prototype.exec = poison;
    Uint8Array.prototype.set = poison; Array.prototype.push = poison;
    globalThis.Uint8Array = poison; globalThis.SyntaxError = poison;
    const hex = bytes.toHex(), encoded = bytes.toBase64();
    const written = into.setFromBase64("AP8=").written;
    const decoded = U8.fromHex("00ff");
    let correctError = false;
    try { U8.fromHex("gg"); } catch (error) { correctError = error instanceof Syntax; }
    [hex, encoded, written, into[1], decoded[1], correctError];
  `);
  assert.deepEqual(Array.from(result), ["00ff", "AP8=", 2, 255, 255, true]);
});

void test("set algebra protects its receiver operations but reads the other set-like record", () => {
  const { run } = realm("delete Set.prototype.union; delete Set.prototype.intersection;");
  const result = run<unknown[]>(`
    const input = new Set([1, 2]);
    const values = Set.prototype.values, next = Object.getPrototypeOf(input.values()).next;
    let reads = 0;
    const other = { get size() { reads++; return 1; }, has(value) { return value === 2; }, keys() { return [2, 3].values(); } };
    const poison = () => { throw new Error("poisoned method"); };
    Set.prototype.add = poison; Set.prototype.has = poison; Set.prototype.delete = poison;
    Set.prototype.values = poison; Set.prototype[Symbol.iterator] = poison;
    Object.getPrototypeOf(Reflect.apply(values, input, [])).next = poison;
    const union = input.union(other), intersection = input.intersection(other);
    const unionIter = Reflect.apply(values, union, []), interIter = Reflect.apply(values, intersection, []);
    [Reflect.apply(next, unionIter, []).value, Reflect.apply(next, unionIter, []).value,
      Reflect.apply(next, unionIter, []).value, Reflect.apply(next, interIter, []).value, reads];
  `);
  assert.deepEqual(Array.from(result), [1, 2, 3, 2, 2]);
});
