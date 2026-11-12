import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";

const file = fileURLToPath(new URL("../runtime/polyfills.cjs", import.meta.url));
// Each family gets a fresh process: deleting native APIs must not contaminate
// node:test or make a later case pass because an earlier case installed it.
function shimTest(name: string, body: string): void {
  void test(name, () => {
    const script = `
      import assert from 'node:assert/strict';
      import { createRequire } from 'node:module';
      import vm from 'node:vm';
      const require = createRequire(import.meta.url);
      const { installSyncPolyfills: install } = require(${JSON.stringify(file)});
      const remove = (object, ...keys) => { for (const key of keys) assert.equal(delete object[key], true); };
      await (async () => { ${body} })();
    `;
    const result = spawnSync(process.execPath, ["--input-type=module", "-e", script], { encoding: "utf8", timeout: 10000 });
    assert.ifError(result.error);
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  });
}

shimTest("reportError schedules the original error and is non-enumerable", `
  remove(globalThis, "reportError"); install();
  const error = new Error("reported");
  const caught = new Promise((resolve) => process.once("uncaughtException", resolve));
  assert.equal(reportError(error), undefined);
  assert.equal(await caught, error);
  assert.equal(Object.getOwnPropertyDescriptor(globalThis, "reportError").enumerable, false);
`);

shimTest("localStorage neutralization removes a throwing configurable getter", `
  let reads = 0;
  Object.defineProperty(globalThis, "localStorage", { get() { reads++; throw new Error("unconfigured"); }, configurable: true });
  process.env.__NUB_NEUTRALIZE_LOCALSTORAGE = "1";
  install();
  assert.equal("localStorage" in globalThis, false);
  assert.equal(reads, 0);
  install();
  assert.equal("localStorage" in globalThis, false);
`);

shimTest("File and Blob bridge node:buffer identities when globals are absent", `
  remove(globalThis, "File", "Blob"); install();
  const buffer = require("node:buffer");
  assert.equal(File, buffer.File); assert.equal(Blob, buffer.Blob);
  const file = new File(["hello"], "example.txt");
  assert.equal(file.name, "example.txt"); assert.equal(await file.text(), "hello");
  assert.equal(Object.getOwnPropertyDescriptor(globalThis, "File").enumerable, false);
`);

shimTest("URLPattern and Float16 adapters delegate to their supplied packages", `
  remove(globalThis, "URLPattern", "Float16Array");
  remove(DataView.prototype, "getFloat16", "setFloat16"); remove(Math, "f16round");
  const calls = [];
  class Pattern {}
  class F16 {}
  const round = (value) => value / 2;
  install({ urlpattern: { URLPattern: Pattern }, float16: {
    Float16Array: F16, f16round: round,
    getFloat16(...args) { calls.push(args); return 1.5; },
    setFloat16(...args) { calls.push(args); },
  } });
  assert.equal(URLPattern, Pattern); assert.equal(Float16Array, F16); assert.equal(Math.f16round(3), 1.5);
  // Defined as the engine defines them: non-enumerable, the functions as methods
  // with the optional littleEndian left out of length.
  const shapes = [[globalThis, "URLPattern"], [globalThis, "Float16Array"], [Math, "f16round", 1],
    [DataView.prototype, "getFloat16", 1], [DataView.prototype, "setFloat16", 2]];
  for (const [object, key, length] of shapes) {
    const { value, enumerable } = Object.getOwnPropertyDescriptor(object, key);
    assert.equal(enumerable, false, key);
    if (length === undefined) continue;
    assert.equal(value.length, length, key); assert.equal(value.name, key);
    assert.equal(Object.hasOwn(value, "prototype"), false, key);
  }
  const view = new DataView(new ArrayBuffer(8));
  assert.equal(view.getFloat16(2, true), 1.5);
  view.setFloat16(4, -1.5, false);
  assert.deepEqual(calls, [[view, 2, true], [view, 4, -1.5, false]]);
  const get = view.getFloat16; install({ float16: { Float16Array: class Replacement {} } });
  assert.equal(view.getFloat16, get); assert.equal(Float16Array, F16);
`);

shimTest("URLPattern assigned by its package's own entry is made non-enumerable", `
  remove(globalThis, "URLPattern");
  class Pattern {}
  globalThis.URLPattern = Pattern;
  install({ urlpattern: { URLPattern: Pattern } });
  assert.equal(URLPattern, Pattern);
  assert.equal(Object.getOwnPropertyDescriptor(globalThis, "URLPattern").enumerable, false);
`);

shimTest("polyfilled methods are methods: no prototype, not constructors, not enumerable", `
  const TypedArray = Object.getPrototypeOf(Int8Array);
  const targets = [
    [Iterator, "from", "concat"],
    [Map.prototype, "getOrInsert", "getOrInsertComputed"],
    [WeakMap.prototype, "getOrInsert", "getOrInsertComputed"],
    [URL, "parse"],
    [String.prototype, "isWellFormed", "toWellFormed"],
    [Object, "groupBy"],
    [Map, "groupBy"],
    [Array.prototype, "toSorted", "toReversed", "toSpliced"],
    [TypedArray.prototype, "toSorted", "toReversed"],
    [ArrayBuffer.prototype, "transfer", "transferToFixedLength"],
    [Atomics, "pause"],
    [Uint8Array, "fromBase64", "fromHex"],
    [Uint8Array.prototype, "toBase64", "setFromBase64", "toHex", "setFromHex"],
  ];
  for (const [object, ...keys] of targets) remove(object, ...keys);
  remove(ArrayBuffer.prototype, "detached");
  install();
  const source = (fn) => Function.prototype.toString.call(fn);
  for (const [object, ...keys] of targets) {
    for (const key of keys) {
      const { value, enumerable } = Object.getOwnPropertyDescriptor(object, key);
      // The polyfill is what is measured, not a native the removal missed.
      assert.doesNotMatch(source(value), /\\[native code\\]/, key);
      assert.equal(enumerable, false, key);
      assert.equal(value.name, key);
      assert.equal(Object.hasOwn(value, "prototype"), false, key);
      assert.throws(() => Reflect.construct(Object, [], value), TypeError, key);
    }
  }
  const detached = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, "detached").get;
  assert.doesNotMatch(source(detached), /\\[native code\\]/);
  assert.equal(detached.name, "get detached");
  assert.equal(Object.hasOwn(detached, "prototype"), false);
`);

shimTest("RegExp.escape protects concatenation, punctuation, whitespace, lone surrogates and astral text", `
  remove(RegExp, "escape"); install();
  assert.equal(RegExp.escape("a-b.c\\n"), "\\\\x61\\\\x2db\\\\.c\\\\n");
  assert.equal(RegExp.escape(" \\u2028"), "\\\\x20\\\\u2028");
  assert.equal(RegExp.escape("😀"), "😀");
  assert.equal(RegExp.escape("\\udc00x\\ud83d"), "\\\\udc00x\\\\ud83d");
  for (const text of ["foo(bar)", "[x]", "1+1", "a\\\\b", "😀"]) {
    assert.equal(new RegExp(\`^(?:\${RegExp.escape(text)})$\`, "u").test(text), true);
  }
  assert.throws(() => RegExp.escape({ toString: () => "x" }), TypeError);
  const { value, enumerable } = Object.getOwnPropertyDescriptor(RegExp, "escape");
  assert.deepEqual([enumerable, value.name, value.length], [false, "escape", 1]);
`);

shimTest("Error.isError rejects tag spoofs and recognizes local subclasses", `
  remove(Error, "isError"); install();
  class Custom extends Error {}
  assert.equal(Error.isError(new Custom()), true);
  for (const value of [null, undefined, 1, "Error", {}, { [Symbol.toStringTag]: "Error" }]) assert.equal(Error.isError(value), false);
  const { value, enumerable } = Object.getOwnPropertyDescriptor(Error, "isError");
  assert.deepEqual([enumerable, value.name, value.length], [false, "isError", 1]);
`);

shimTest("Set operations use set-like inputs, membership and size-aware ordering", `
  remove(Set.prototype, "union", "intersection", "difference", "symmetricDifference", "isSubsetOf", "isSupersetOf", "isDisjointFrom"); install();
  const left = new Set([1, 2]), right = new Map([[2, "x"], [3, "y"]]);
  assert.deepEqual([...left.union(right)], [1, 2, 3]);
  assert.deepEqual([...left.intersection(right)], [2]);
  assert.deepEqual([...left.difference(right)], [1]);
  assert.deepEqual([...left.symmetricDifference(right)], [1, 3]);
  assert.equal(left.isSubsetOf(new Set([1, 2, 3])), true);
  assert.equal(left.isSupersetOf(new Set([2])), true);
  assert.equal(left.isDisjointFrom(new Set([3])), true);
  assert.equal(left.isDisjointFrom(right), false);
  assert.throws(() => left.union({ size: NaN, has() {}, keys() {} }), TypeError);
  assert.throws(() => Set.prototype.union.call({}, right), TypeError);
`);

shimTest("Set operations step a set-like's keys() result directly and close it on early exit", `
  remove(Set.prototype, "union", "intersection", "difference", "symmetricDifference", "isSubsetOf", "isSupersetOf", "isDisjointFrom"); install();
  const log = [];
  const like = (values) => ({ size: values.length, has: (v) => values.includes(v), keys() {
    let i = 0;
    return { next() { return i < values.length ? { done: false, value: values[i++] } : { done: true }; }, return() { log.push("return"); return {}; } };
  } });
  assert.deepEqual([...new Set([1, 2]).union(like([2, 3]))], [1, 2, 3]);
  assert.equal(new Set([1, 2, 3]).isSupersetOf(like([1, 9, 2])), false);
  assert.deepEqual(log, ["return"]);
  assert.throws(() => new Set([1, 2]).union({ size: 1, has() {}, keys: () => "ab" }), TypeError);
`);

shimTest("Array.fromAsync awaits iterable, array-like and mapped values", `
  remove(Array, "fromAsync"); install();
  class Sub extends Array {}
  const context = { offset: 3 };
  const result = await Array.fromAsync.call(Sub, [Promise.resolve(1), 2], async function (v, i) { return v + i + this.offset; }, context);
  assert.equal(result instanceof Sub, true); assert.deepEqual([...result], [4, 6]);
  assert.deepEqual(await Array.fromAsync({ 0: Promise.resolve(5), length: 1 }), [5]);
  assert.deepEqual(await Array.fromAsync({ length: -1 }), []);
  let closed = false;
  async function* source() { try { yield 1; yield 2; } finally { closed = true; } }
  await assert.rejects(Array.fromAsync(source(), () => { throw new Error("map"); }), /map/);
  assert.equal(closed, true);
`);

shimTest("Map and WeakMap upsert handle present undefined, reentrancy and invalid keys", `
  for (const ctor of [Map, WeakMap]) remove(ctor.prototype, "getOrInsert", "getOrInsertComputed"); install();
  for (const ctor of [Map, WeakMap]) {
    const map = new ctor(), key = {};
    assert.equal(map.getOrInsert(key, undefined), undefined);
    assert.equal(map.getOrInsertComputed(key, () => { throw new Error("not called"); }), undefined);
    map.delete(key);
    assert.equal(map.getOrInsertComputed(key, (received) => { assert.equal(received, key); map.set(key, "inner"); return "outer"; }), "outer");
    assert.equal(map.get(key), "outer");
    assert.throws(() => map.getOrInsertComputed(key, null), TypeError);
  }
  assert.throws(() => new WeakMap().getOrInsert(1, "value"), TypeError);
`);

shimTest("Promise.withResolvers and keyed combinators preserve constructors, keys and failures", `
  remove(Promise, "withResolvers", "allKeyed", "allSettledKeyed"); install();
  class Sub extends Promise {}
  const capability = Sub.withResolvers(); assert.equal(capability.promise instanceof Sub, true);
  capability.resolve(42); assert.equal(await capability.promise, 42);
  const symbol = Symbol("key"), input = { a: Promise.resolve(1), [symbol]: 2 };
  Object.defineProperty(input, "hidden", { value: Promise.reject(new Error("unused")), enumerable: false });
  input.hidden.catch(() => {});
  const promise = Sub.allKeyed(input); assert.equal(promise instanceof Sub, true);
  const values = await promise;
  assert.equal(Object.getPrototypeOf(values), null); assert.deepEqual(Reflect.ownKeys(values), ["a", symbol]); assert.equal(values[symbol], 2);
  const error = new Error("reject");
  await assert.rejects(Promise.allKeyed({ a: Promise.reject(error) }), (e) => e === error);
  assert.deepEqual({ ...await Promise.allSettledKeyed({ ok: 1, bad: Promise.reject(error) }) }, { ok: { status: "fulfilled", value: 1 }, bad: { status: "rejected", reason: error } });
  await assert.rejects(Promise.allKeyed(null), TypeError);
  assert.equal(Object.keys(await Promise.allKeyed({})).length, 0);
`);

shimTest("Promise.withResolvers and keyed combinators are non-constructors like builtin methods", `
  remove(Promise, "withResolvers", "allKeyed", "allSettledKeyed"); install();
  for (const [key, length] of [["withResolvers", 0], ["allKeyed", 1], ["allSettledKeyed", 1]]) {
    const fn = Promise[key];
    assert.equal(Object.hasOwn(fn, "prototype"), false, key);
    assert.deepEqual(Reflect.ownKeys(fn), ["length", "name"], key);
    assert.throws(() => Reflect.construct(Object, [], fn), TypeError, key);
    assert.throws(() => new fn(), TypeError, key);
    assert.equal(fn.name, key); assert.equal(fn.length, length, key);
    assert.deepEqual(Object.getOwnPropertyDescriptor(Promise, key), { value: fn, writable: true, enumerable: false, configurable: true }, key);
  }
`);

shimTest("JSON options freeze recursively, isolate prototypes and preserve revivers", `
  const parse = JSON.parse; install();
  const value = JSON.parse('{"nested":{"x":1},"array":[{"y":2}]}', { freeze: true });
  assert.equal(Object.getPrototypeOf(value), null); assert.equal(Object.getPrototypeOf(value.nested), null);
  assert.equal(Object.isFrozen(value.array), true); assert.equal(Object.isFrozen(value.array[0]), true);
  assert.equal(Array.isArray(value.array), true);
  const plain = JSON.parse('{"x":1}', { freeze: true, preferNullPrototype: false });
  assert.equal(Object.getPrototypeOf(plain), Object.prototype);
  assert.equal(JSON.parse('{"x":1}', (key, v) => key === "x" ? 2 : v).x, 2);
  assert.throws(() => JSON.parse("{}", { freeze: 1 }), TypeError);
  assert.throws(() => JSON.parse("{", { freeze: true }), SyntaxError);
  assert.deepEqual(JSON.parse('{"x":1}'), parse('{"x":1}'));
`);

shimTest("well-formed strings repair only unpaired UTF-16 surrogates", `
  remove(String.prototype, "isWellFormed", "toWellFormed"); install();
  assert.equal("😀".isWellFormed(), true); assert.equal("\\ud800x\\udc00".isWellFormed(), false);
  assert.equal("\\ud800x\\udc00".toWellFormed(), "\\ufffdx\\ufffd");
  assert.equal(String.prototype.toWellFormed.call(123), "123");
  assert.throws(() => String.prototype.isWellFormed.call(null), TypeError);
`);

shimTest("groupBy handles special string keys, identity keys and iterator closure", `
  remove(Object, "groupBy"); remove(Map, "groupBy"); install();
  const grouped = Object.groupBy([1, 2], () => "__proto__");
  assert.equal(Object.getPrototypeOf(grouped), null); assert.deepEqual(grouped.__proto__, [1, 2]);
  const key = {}; assert.deepEqual(Map.groupBy([1, 2], () => key).get(key), [1, 2]);
  let closed = false;
  function* source() { try { yield 1; } finally { closed = true; } }
  assert.throws(() => Object.groupBy(source(), () => { throw new Error("key"); }), /key/); assert.equal(closed, true);
`);

shimTest("copying Array and TypedArray methods preserve sources and index semantics", `
  const typed = Object.getPrototypeOf(Int8Array.prototype);
  remove(Array.prototype, "toSorted", "toReversed", "toSpliced", "with"); remove(typed, "toSorted", "toReversed", "with"); install();
  const array = [3, 1, 2];
  assert.deepEqual(array.toSorted(), [1, 2, 3]); assert.deepEqual(array.toReversed(), [2, 1, 3]);
  assert.deepEqual(array.toSpliced(1), [3]); assert.deepEqual(array.toSpliced(1, undefined), array);
  assert.deepEqual(array.with(-1, 9), [3, 1, 9]); assert.deepEqual(array, [3, 1, 2]);
  assert.throws(() => array.with(3, 0), RangeError);
  const bytes = new Uint8Array([10, 2, 1]);
  assert.deepEqual([...bytes.toSorted()], [1, 2, 10]); assert.deepEqual([...bytes.toReversed()], [1, 2, 10]);
  assert.deepEqual([...bytes.with(-1, 7)], [10, 2, 7]); assert.deepEqual([...bytes], [10, 2, 1]);
`);

shimTest("ArrayBuffer transfer detaches and preserves resize policy", `
  remove(ArrayBuffer.prototype, "transfer", "transferToFixedLength", "detached"); install();
  const buffer = new ArrayBuffer(2, { maxByteLength: 8 }); new Uint8Array(buffer).set([1, 2]);
  const moved = buffer.transfer(4);
  assert.equal(buffer.detached, true); assert.deepEqual([...new Uint8Array(moved)], [1, 2, 0, 0]); assert.equal(moved.resizable, true);
  assert.throws(() => buffer.transfer(), TypeError);
  const fixed = moved.transferToFixedLength(1); assert.equal(fixed.resizable, false); assert.deepEqual([...new Uint8Array(fixed)], [1]);
  assert.equal(new ArrayBuffer(0).detached, false);
`);

shimTest("URL.parse and Atomics.pause validate inputs without side effects", `
  remove(URL, "parse"); remove(Atomics, "pause"); install();
  assert.equal(URL.parse("/x", "https://example.com").href, "https://example.com/x");
  assert.equal(URL.parse("not a url"), null);
  assert.equal(URL.parse(undefined), null); assert.throws(() => URL.parse(), TypeError);
  assert.throws(() => URL.parse(Symbol()), TypeError);
  assert.throws(() => URL.parse("https://example.com", { toString() { throw new Error("base coercion"); } }), /base coercion/);
  const error = new Error("coercion"); assert.throws(() => URL.parse({ toString() { throw error; } }), (e) => e === error);
  assert.equal(Atomics.pause(), undefined); assert.equal(Atomics.pause(0), undefined);
  for (const value of [1.5, Infinity, "1", 1n]) assert.throws(() => Atomics.pause(value), TypeError);
`);

shimTest("Uint8Array codecs handle alphabets, partial writes and invalid input", `
  remove(Uint8Array, "fromBase64", "fromHex"); remove(Uint8Array.prototype, "toBase64", "setFromBase64", "toHex", "setFromHex"); install();
  const bytes = new Uint8Array([251, 255]);
  assert.equal(bytes.toBase64(), "+/8="); assert.equal(bytes.toBase64({ alphabet: "base64url", omitPadding: true }), "-_8");
  assert.deepEqual([...Uint8Array.fromBase64("-_8", { alphabet: "base64url" })], [251, 255]);
  assert.equal(bytes.toHex(), "fbff"); assert.deepEqual([...Uint8Array.fromHex("FbFF")], [251, 255]);
  const target = new Uint8Array(1);
  assert.deepEqual(target.setFromHex("0102"), { read: 2, written: 1 }); assert.equal(target[0], 1);
  assert.deepEqual(target.setFromBase64("YQ=="), { read: 4, written: 1 }); assert.equal(target[0], 97);
  assert.throws(() => Uint8Array.fromHex("x0"), SyntaxError); assert.throws(() => Uint8Array.fromHex("0"), SyntaxError);
  assert.throws(() => Uint8Array.fromBase64("!"), SyntaxError);
`);

shimTest("disposable stacks move ownership, dispose once and suppress failures in LIFO order", `
  remove(globalThis, "DisposableStack", "AsyncDisposableStack", "SuppressedError"); install();
  const log = [], stack = new DisposableStack();
  stack.use({ [Symbol.dispose]() { log.push("use"); } }); stack.adopt(1, (value) => log.push(value)); stack.defer(() => log.push("defer"));
  const moved = stack.move(); assert.equal(stack.disposed, true); stack.dispose(); moved.dispose(); moved.dispose();
  assert.deepEqual(log, ["defer", 1, "use"]); assert.throws(() => stack.defer(() => {}), ReferenceError);
  const first = new Error("first"), second = new Error("second"), errors = new DisposableStack();
  errors.defer(() => { throw first; }); errors.defer(() => { throw second; });
  assert.throws(() => errors.dispose(), (e) => e instanceof SuppressedError && e.error === first && e.suppressed === second);
  const asyncStack = new AsyncDisposableStack();
  asyncStack.use({ async [Symbol.asyncDispose]() { await 0; log.push("async"); } });
  asyncStack.use({ [Symbol.dispose]() { log.push("sync"); } });
  asyncStack.adopt(2, async (value) => { await 0; log.push(value); });
  const asyncMoved = asyncStack.move(); await asyncMoved.disposeAsync(); await asyncMoved.disposeAsync();
  assert.deepEqual(log.slice(3), [2, "sync", "async"]); assert.equal(asyncStack.disposed, true);
  assert.throws(() => asyncMoved.use({}), ReferenceError);
`);

shimTest("Iterator constructor, core helpers, concat, chunks/windows and disposal work without native helpers", `
  const ip = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
  remove(globalThis, "Iterator");
  remove(ip, "map", "filter", "take", "drop", "flatMap", "reduce", "toArray", "some", "every", "find", "forEach", "chunks", "windows", "join", "includes", Symbol.dispose);
  install();
  assert.throws(() => new Iterator(), TypeError); class Sub extends Iterator {} assert.equal(new Sub() instanceof Iterator, true);
  assert.deepEqual(Iterator.from([1, 2, 3]).map((x) => x * 2).filter((x) => x > 2).toArray(), [4, 6]);
  assert.deepEqual(Iterator.from([1, 2]).flatMap((x) => [x, x]).toArray(), [1, 1, 2, 2]);
  assert.equal(Iterator.from([1, 2, 3]).reduce((a, b) => a + b, 0), 6);
  assert.equal(Iterator.from([1, 2]).some((x) => x === 2), true); assert.equal(Iterator.from([1, 2]).every((x) => x > 0), true); assert.equal(Iterator.from([1, 2]).find((x) => x > 1), 2);
  const seen = []; Iterator.from([1, 2]).forEach((v, i) => seen.push([v, i])); assert.deepEqual(seen, [[1, 0], [2, 1]]);
  assert.deepEqual(Iterator.concat([1], [2, 3]).toArray(), [1, 2, 3]);
  assert.deepEqual(Iterator.from([1, 2, 3]).chunks(2).toArray(), [[1, 2], [3]]);
  assert.deepEqual(Iterator.from([1, 2, 3]).windows(2).toArray(), [[1, 2], [2, 3]]);
  let closed = 0; ip[Symbol.dispose].call({ return() { closed++; } }); assert.equal(closed, 1);
`);

shimTest("Iterator.concat rejects primitives, captures each method up front and steps sources with a bare next()", `
  remove(Iterator, "concat"); install();
  assert.throws(() => Iterator.concat("ab"), TypeError);
  const nextArgs = [];
  const source = { [Symbol.iterator]() { let i = 0; return { next(...args) { nextArgs.push(args.length); return { done: i > 1, value: i++ }; } }; } };
  const it = Iterator.concat(source);
  source[Symbol.iterator] = () => assert.fail("method read after the call");
  assert.equal(Object.prototype.toString.call(it), "[object Iterator Helper]");
  assert.equal(it.next("x").value, 0); assert.equal(it.next("y").value, 1);
  assert.deepEqual(nextArgs, [0, 0]);
`);

shimTest("Symbol.metadata has the standard immutable, hidden descriptor", `
  // A host's existing well-known symbol may be non-configurable; preserve it.
  const before = Symbol.metadata; install();
  assert.equal(typeof Symbol.metadata, "symbol");
  if (before !== undefined) assert.equal(Symbol.metadata, before);
  const desc = Object.getOwnPropertyDescriptor(Symbol, "metadata");
  assert.equal(desc.enumerable, false); assert.equal(desc.writable, false); assert.equal(desc.configurable, false);
`);

shimTest("Float16 independently preserves either native DataView half", `
  const sentinel = function () { return "native"; };
  for (const [present, missing] of [["getFloat16", "setFloat16"], ["setFloat16", "getFloat16"]]) {
    remove(globalThis, "Float16Array"); remove(DataView.prototype, missing);
    Object.defineProperty(DataView.prototype, present, { value: sentinel, writable: true, configurable: true });
    install({ float16: { Float16Array: class F16 {}, getFloat16() { return 2; }, setFloat16() {}, f16round: (x) => x } });
    assert.equal(DataView.prototype[present], sentinel);
    assert.equal(typeof DataView.prototype[missing], "function");
  }
`);

shimTest("MessageEvent ports patch freezes old hosts and leaves modern lazy globals untouched", `
  class OldEvent { get ports() { return this.values; } }
  globalThis.MessageEvent = OldEvent;
  Object.defineProperty(process.versions, "node", { value: "20.19.0", configurable: true });
  install();
  const event = new OldEvent(); event.values = [1];
  assert.equal(Object.isFrozen(event.ports), true);
  const getter = Object.getOwnPropertyDescriptor(OldEvent.prototype, "ports").get;
  install(); assert.equal(Object.getOwnPropertyDescriptor(OldEvent.prototype, "ports").get, getter);
  event.values = null; assert.equal(event.ports, null);
  let reads = 0;
  Object.defineProperty(globalThis, "MessageEvent", { get() { reads++; throw new Error("lazy global read"); }, configurable: true });
  Object.defineProperty(process.versions, "node", { value: "24.22.1", configurable: true });
  install(); assert.equal(reads, 0);
`);

shimTest("all shared method installers preserve supplied implementations", `
  const ip = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
  const aip = Object.getPrototypeOf(Object.getPrototypeOf(Object.getPrototypeOf((async function* () {})())));
  const typed = Object.getPrototypeOf(Uint8Array.prototype);
  const targets = [
    [globalThis, ["reportError", "File", "Blob", "URLPattern", "Float16Array", "DisposableStack", "AsyncDisposableStack", "SuppressedError"]],
    [RegExp, ["escape"]], [Error, ["isError"]], [Math, ["sumPrecise", "f16round"]],
    [DataView.prototype, ["getFloat16", "setFloat16"]],
    [Promise, ["try", "withResolvers", "allKeyed", "allSettledKeyed"]],
    [Set.prototype, ["union", "intersection", "difference", "symmetricDifference", "isSubsetOf", "isSupersetOf", "isDisjointFrom"]],
    [Array, ["fromAsync"]], [Object, ["groupBy"]], [Map, ["groupBy"]],
    [Map.prototype, ["getOrInsert", "getOrInsertComputed"]], [WeakMap.prototype, ["getOrInsert", "getOrInsertComputed"]],
    [String.prototype, ["isWellFormed", "toWellFormed"]],
    [Array.prototype, ["toSorted", "toReversed", "toSpliced", "with"]], [typed, ["toSorted", "toReversed", "with"]],
    [ArrayBuffer.prototype, ["transfer", "transferToFixedLength"]], [URL, ["parse"]], [Atomics, ["pause"]],
    [Uint8Array, ["fromBase64", "fromHex"]], [Uint8Array.prototype, ["toBase64", "setFromBase64", "toHex", "setFromHex"]],
    [Iterator, ["from", "concat", "zip", "zipKeyed"]],
    [ip, ["map", "filter", "take", "drop", "flatMap", "reduce", "toArray", "some", "every", "find", "forEach", "chunks", "windows", "includes", "join", Symbol.dispose]],
    [aip, [Symbol.asyncDispose]],
  ];
  const planted = [];
  for (const [target, keys] of targets) for (const key of keys) {
    const sentinel = function sentinel() {};
    Object.defineProperty(target, key, { value: sentinel, writable: true, configurable: true, enumerable: false });
    planted.push([target, key, sentinel]);
  }
  const variants = function () { return "native"; };
  Object.defineProperty(Intl.Locale.prototype, "variants", { get: variants, configurable: true });
  const detached = function () { return false; };
  Object.defineProperty(ArrayBuffer.prototype, "detached", { get: detached, configurable: true });
  install();
  for (const [target, key, sentinel] of planted) assert.equal(target[key], sentinel, String(key));
  assert.equal(Object.getOwnPropertyDescriptor(Intl.Locale.prototype, "variants").get, variants);
  assert.equal(Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, "detached").get, detached);
  const parse = JSON.parse; install(); assert.equal(JSON.parse, parse);
`);
