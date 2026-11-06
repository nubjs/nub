// This package declares global APIs rather than an importable module.
// oxlint-disable-next-line typescript/triple-slash-reference
/// <reference path="../npm/nub-types/index.d.ts" />
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import test from "node:test";


interface JointIterator<T = unknown> {
  next(): IteratorResult<T, undefined>;
  return(): IteratorResult<T, undefined>;
  toArray(): T[];
}
interface JointOptions { mode?: "shortest" | "longest" | "strict"; padding?: unknown; }
// Unknown inputs are intentional: these runtime tests also check invalid arguments.
const joint = Iterator as unknown as {
  zip(this: void, input: unknown, options?: JointOptions): JointIterator<unknown[]>;
  zipKeyed(this: void, input: unknown, options?: JointOptions): JointIterator<Record<PropertyKey, unknown>>;
};

// Run this same corpus with NUB_TEST_NATIVE=1 on Node 27 for a native oracle.
// Otherwise remove only the methods under test so every Node exercises fallbacks.
const native = process.env.NUB_TEST_NATIVE === "1";
const polyfills = fileURLToPath(new URL("../runtime/polyfills.cjs", import.meta.url));
const ip = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
const aip = Object.getPrototypeOf(Object.getPrototypeOf(Object.getPrototypeOf((async function* () {})())));
if (!native) {
  for (const [holder, keys] of [
    [Math, ["sumPrecise"]], [Intl.Locale.prototype, ["variants"]],
    [Promise, ["try"]], [ip, ["join", "includes", "take", "drop"]],
    [aip, [Symbol.asyncDispose]],
  ] as [object, PropertyKey[]][]) for (const key of keys) Reflect.deleteProperty(holder, key);
  if (globalThis.Iterator) for (const key of ["zip", "zipKeyed"]) Reflect.deleteProperty(Iterator, key);
  createRequire(import.meta.url)(polyfills).installSyncPolyfills();
}

void test("locale variants use the intrinsic locale, brand-check and omit extensions", () => {
  // The accessor is deliberately extracted for explicit receiver and brand checks.
  // oxlint-disable-next-line typescript/unbound-method
  const getter = Object.getOwnPropertyDescriptor(Intl.Locale.prototype, "variants")!.get!;
  for (const [tag, expected] of [
    ["en", undefined], ["sl-rozaj-biske", "biske-rozaj"],
    ["de-Latn-DE-1901-u-ca-gregory-x-foobar", "1901"],
    ["en-US-u-nu-latn", undefined], ["en-abcde-fghij", "abcde-fghij"],
  ] as [string, string | undefined][]) assert.equal(new Intl.Locale(tag).variants, expected);
  const locale = new Intl.Locale("sl-rozaj");
  Object.defineProperty(locale, "baseName", { get() { throw new Error("override"); } });
  locale.toString = () => { throw new Error("override"); };
  assert.equal(locale.variants, "rozaj");
  assert.equal(getter.call(vm.runInNewContext('new Intl.Locale("de-1901")')), "1901");
  for (const receiver of [null, undefined, {}, "en", new Proxy(locale, {})]) {
    assert.throws(() => getter.call(receiver), TypeError);
  }
  const desc = Object.getOwnPropertyDescriptor(Intl.Locale.prototype, "variants")!;
  assert.equal(desc.enumerable, false);
  assert.equal(desc.configurable, true);
  // The descriptor setter is inspected for absence, never called.
  // oxlint-disable-next-line typescript/unbound-method
  assert.equal(desc.set, undefined);
  assert.equal(getter.name, "get variants");
});

void test("the variants option sets, replaces, sorts and validates variants", () => {
  // Non-string and invalid values are intentional here.
  const Locale = Intl.Locale as unknown as new (tag: string, options?: unknown) => Intl.Locale;
  for (const [tag, variants, expected] of [
    ["sl", "rozaj", "sl-rozaj"], ["sl-biske", "rozaj", "sl-rozaj"], ["sl", "rozaj-biske", "sl-biske-rozaj"],
    ["de-Latn-DE-u-ca-gregory", 1901, "de-Latn-DE-1901-u-ca-gregory"], ["sl-rozaj", undefined, "sl-rozaj"],
  ] as [string, unknown, string][]) assert.equal(new Locale(tag, { variants }).toString(), expected);
  for (const variants of ["", "x", "GB-scouse", "rozaj-", "rozaj-Rozaj"]) {
    assert.throws(() => new Locale("sl", { variants }), RangeError);
  }
  assert.throws(() => new Locale("sl", { variants: Symbol() }), TypeError);
  let reads = 0;
  assert.equal(new Locale("sl", { get variants() { reads++; return "rozaj"; } }).variants, "rozaj");
  assert.equal(reads, 1);
  const order: PropertyKey[] = [];
  new Locale("sl", new Proxy<Record<PropertyKey, unknown>>({ variants: "rozaj" }, { get(target, key) { order.push(key); return target[key]; } }));
  assert.ok(order.indexOf("region") < order.indexOf("variants") && order.indexOf("variants") < order.indexOf("calendar"), order.map(String).join());
  class Sub extends Intl.Locale {}
  assert.ok(new (Sub as unknown as typeof Locale)("sl", { variants: "rozaj" }) instanceof Sub);
  assert.ok(new Sub("sl", { region: "IT" }) instanceof Sub);
  for (const options of [undefined, { variants: "rozaj" }]) {
    let prototypeReads = 0;
    const first = {}, second = {};
    const newTarget = new Proxy(function LocaleTarget() {}, {
      get(target, key, receiver) {
        if (key === "prototype") return ++prototypeReads === 1 ? first : second;
        return Reflect.get(target, key, receiver);
      },
    });
    const instance = Reflect.construct(Locale, ["sl", options], newTarget);
    assert.equal(prototypeReads, 1);
    assert.equal(Object.getPrototypeOf(instance), first);

    let throwReads = 0;
    const throwingTarget = new Proxy(function LocaleTarget() {}, {
      get(target, key, receiver) {
        if (key === "prototype" && ++throwReads > 1) throw new Error("prototype read twice");
        return Reflect.get(target, key, receiver);
      },
    });
    assert.doesNotThrow(() => Reflect.construct(Locale, ["sl", options], throwingTarget));
    assert.equal(throwReads, 1);
  }
  let calendarReads = 0;
  assert.throws(() => new Locale("sl", {
    variants: "rozaj-Rozaj",
    get calendar() { calendarReads++; return "gregory"; },
  }), RangeError);
  assert.equal(calendarReads, 0);
  assert.ok(new Intl.Locale("en") instanceof Intl.Locale);
  assert.equal(Intl.Locale.prototype.constructor, Intl.Locale);
  assert.equal(Intl.Locale.name, "Locale");
  assert.equal(Intl.Locale.length, 1);
  assert.throws(() => (Intl.Locale as unknown as (tag: string) => unknown)("en"), TypeError);
});

void test("polyfills leave frozen intrinsics alone so the program still starts", () => {
  const script = `require(${JSON.stringify(polyfills)}).installSyncPolyfills();`;
  const result = spawnSync(process.execPath, ["--frozen-intrinsics", "--no-warnings", "-e", script], { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
});

void test("polyfills still install when a preload froze only Object.prototype", () => {
  const script = `
    delete Math.sumPrecise;
    Object.freeze(Object.prototype);
    require(${JSON.stringify(polyfills)}).installSyncPolyfills();
    process.stdout.write(String(Math.sumPrecise([1, 2])));
  `;
  const result = spawnSync(process.execPath, ["-e", script], { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, "3");
});

void test("polyfills install on a Node with no Intl", () => {
  const script = `delete globalThis.Intl; require(${JSON.stringify(polyfills)}).installSyncPolyfills();`;
  const result = spawnSync(process.execPath, ["-e", script], { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
});

void test("precise sums round once, preserve signed zero and survive overflow/cancellation", () => {
  const m = Number.MAX_VALUE, tiny = Number.MIN_VALUE;
  for (const [values, expected] of [
    [[], -0], [[-0, -0], -0], [[-0, 0], 0], [[1, -1], 0],
    [[0.1, 0.2, 0.3], 0.6], [[1, 1e100, 1, -1e100], 2],
    [[m, m, -m, -m, tiny], tiny], [[m, m, -m, -m, -tiny], -tiny],
    [[m, m], Infinity], [[-m, -m], -Infinity],
    [[1, 2 ** -53], 1], [[1, 2 ** -53, tiny], 1 + 2 ** -52],
    [[Infinity, -Infinity], NaN], [[NaN, 1], NaN],
  ] as [number[], number][]) assert.ok(Object.is(Math.sumPrecise(values), expected), `${JSON.stringify(values)}: ${expected}`);
  // @ts-expect-error Deliberately invalid element exercises runtime validation.
  assert.throws(() => Math.sumPrecise([NaN, "1"]), TypeError);
  let closed = false;
  function* values() { try { yield {}; } finally { closed = true; } }
  // @ts-expect-error Deliberately nonnumeric iterable must be closed.
  assert.throws(() => Math.sumPrecise(values()), TypeError);
  assert.equal(closed, true);
});

function tracked(log: string[], name: string, values: unknown[], extra: object = {}) {
  let i = 0;
  return {
    next() { log.push(`${name}.next`); return i < values.length ? { value: values[i++], done: false } : { done: true }; },
    return() { log.push(`${name}.return`); return { done: true }; },
    ...extra,
  };
}

void test("zip supports bare iterators, all modes, null-prototype keyed rows and undefined keys", () => {
  assert.deepEqual(joint.zip([[1, 2], ["a", "b"]]).toArray(), [[1, "a"], [2, "b"]]);
  assert.deepEqual(joint.zip([tracked([], "a", [1]), []]).toArray(), []);
  assert.throws(() => joint.zip(["abc"]), TypeError);
  assert.deepEqual(joint.zip([[1], [2, 3]], { mode: "longest", padding: [0] }).toArray(), [[1, 2], [0, 3]]);
  assert.throws(() => joint.zip([[1], [2, 3]], { mode: "strict" }).toArray(), TypeError);
  const symbol = Symbol("key");
  const row = joint.zipKeyed({ a: undefined, b: [1], [symbol]: [2] }).next().value!;
  assert.equal(Object.getPrototypeOf(row), null);
  assert.deepEqual(Reflect.ownKeys(row), ["b", symbol]);
  assert.equal(row[symbol], 2);
  for (const method of [joint.zip, joint.zipKeyed]) {
    const input = method === joint.zip ? [[1]] : { a: [1] };
    assert.doesNotThrow(() => method(input, { get padding() { throw new Error("unused"); } }).next());
    assert.throws(() => method(input, { mode: "longest", padding: null }), TypeError);
  }
});

void test("zip closes inputs in reverse order, including before first next", () => {
  let log: string[] = [];
  let zipped: JointIterator = joint.zip([tracked(log, "a", [1]), tracked(log, "b", [2])]);
  zipped.return();
  assert.deepEqual(log, ["b.return", "a.return"]);
  log = [];
  zipped = joint.zip([tracked(log, "a", []), tracked(log, "b", [2])]);
  assert.equal(zipped.next().done, true);
  assert.deepEqual(log, ["a.next", "b.return"]);
  log = [];
  zipped = joint.zipKeyed({ a: tracked(log, "a", [1, 2]), b: tracked(log, "b", [3, 4]) });
  zipped.next(); zipped.return();
  assert.deepEqual(log, ["a.next", "b.next", "b.return", "a.return"]);
});

void test("zip snapshots next and padding, skips value access in strict length checks", () => {
  let reads = 0;
  const source = { get next() { reads++; let i = 0; return () => ({ done: i++ > 0, value: 1 }); } };
  assert.deepEqual(joint.zip([source]).toArray(), [[1]]);
  assert.equal(reads, 1);
  let pads = 0;
  const zipped = joint.zipKeyed({ a: [], b: [1, 2] }, { mode: "longest", padding: { get a() { return ++pads; } } });
  assert.equal(pads, 1);
  assert.deepEqual(zipped.toArray().map((row) => row.a), [1, 1]);
  const mismatch = { next() { return { done: false, get value() { throw new Error("value must not be read"); } }; } };
  assert.throws(() => joint.zip([[], mismatch], { mode: "strict" }).next(), TypeError);
});

void test("zip preserves original failures and closes acquired inputs on setup errors", () => {
  const error = new Error("original");
  const log: string[] = [];
  const a = tracked(log, "a", [], { return() { log.push("a.return"); throw new Error("close"); } });
  const b = { get [Symbol.iterator]() { throw error; } };
  assert.throws(() => joint.zip([a, b]), (e) => e === error);
  assert.deepEqual(log, ["a.return"]);
  const c = tracked(log, "c", [], { next() { throw error; } });
  assert.throws(() => joint.zip([c, a]).next(), (e) => e === error);
  assert.deepEqual(log, ["a.return", "a.return"]);
});

void test("join and includes handle NaN, nulls, coercion failures and closing", () => {
  assert.equal([null, undefined, 1].values().join("|"), "||1");
  assert.equal([0, NaN].values().includes(NaN), true);
  assert.equal([1, 2].values().includes(1, 1), false);
  // @ts-expect-error A string index must fail runtime validation.
  assert.throws(() => [1].values().includes(1, "0"), TypeError);
  const log: string[] = [];
  assert.equal(Iterator.prototype.includes.call(tracked(log, "a", [1, 2]), 1), true);
  assert.deepEqual(log, ["a.next", "a.return"]);
  assert.throws(() => [Symbol()].values().join(), TypeError);
});

void test("take/drop reject excessive finite limits, close on validation and permit Infinity", () => {
  for (const name of ["take", "drop"] as const) {
    for (const limit of [2 ** 53, NaN, -1, 1n]) {
      const log: string[] = [];
      // @ts-expect-error The bigint case deliberately violates the public signature.
      assert.throws(() => Iterator.prototype[name].call(tracked(log, "a", [1]), limit), limit === 1n ? TypeError : RangeError);
      assert.deepEqual(log, ["a.return"]);
    }
  }
  const reads: number[] = [];
  const source = { i: 0, next() { const i = this.i++; return { done: i > 2, get value() { reads.push(i); return i; } }; } };
  assert.deepEqual((ip as { drop(limit: number): { toArray(): unknown[] } }).drop.call(source, 2).toArray(), [2]);
  assert.deepEqual(reads, [2]);
  assert.deepEqual([1, 2].values().take(Infinity).toArray(), [1, 2]);
  assert.deepEqual([1, 2].values().drop(Infinity).toArray(), []);
});

void test("Promise.try follows PromiseResolve identity and callback-before-constructor order", async () => {
  const promise = Promise.resolve(1);
  assert.equal(Promise.try(() => promise), promise);
  class Sub extends Promise<number> { static resolve(): never { throw new Error("must not read"); } }
  const sub = new Sub((resolve) => resolve(2));
  assert.equal(Promise.try.call(Sub, () => sub), sub);
  const log: string[] = [];
  function C(executor: ConstructorParameters<typeof Promise<number>>[0]) { log.push("ctor"); return new Promise(executor); }
  await Promise.try.call(C, () => { log.push("callback"); return 3; });
  assert.deepEqual(log, ["callback", "ctor"]);
  const error = new Error("callback");
  await assert.rejects(Promise.try(() => { throw error; }), (e) => e === error);
  let called = false;
  assert.throws(() => Promise.try.call({}, () => { called = true; }), TypeError);
  assert.equal(called, true);
});

void test("async disposal calls return with no arguments and awaits completion", async () => {
  let args: unknown[] | undefined, complete = false;
  await aip[Symbol.asyncDispose].call({ async return(...values: unknown[]) { args = values; await Promise.resolve(); complete = true; } });
  assert.deepEqual(args, []);
  assert.equal(complete, true);
  await assert.rejects(aip[Symbol.asyncDispose].call({ return: 1 }), TypeError);
});

void test("installing polyfills preserves existing native methods and getter identities", () => {
  const script = `
    const assert = require('node:assert/strict');
    const targets = [[Math,'sumPrecise'],[Promise,'try']];
    if (globalThis.Iterator) {
      targets.push([Iterator,'zip'],[Iterator,'zipKeyed']);
      for (const key of ['join','includes','take','drop']) targets.push([Iterator.prototype,key]);
    }
    const before = targets.map(([object,key]) => [object,key,object[key]]).filter(([, , fn]) => typeof fn === 'function');
    // The accessor is deliberately extracted for explicit receiver and brand checks.
  // oxlint-disable-next-line typescript/unbound-method
  const getter = Object.getOwnPropertyDescriptor(Intl.Locale.prototype,'variants')?.get;
    require(${JSON.stringify(polyfills)}).installSyncPolyfills();
    for (const [object,key,fn] of before) assert.equal(object[key],fn);
    if (getter) assert.equal(Object.getOwnPropertyDescriptor(Intl.Locale.prototype,'variants').get,getter);
  `;
  const result = spawnSync(process.execPath, ["-e", script], { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
});

void test("joint helpers guard yielded cleanup reentry but permit completed start cleanup", () => {
  for (const method of [joint.zip, joint.zipKeyed]) {
    for (const started of [false, true]) {
      let helper: JointIterator;
      let closes = 0;
      const source = { next() { return { value: 1, done: false }; }, return() {
        closes++;
        for (const name of ["next", "return"] as const) {
          if (started) assert.throws(() => helper[name](), TypeError);
          else assert.deepEqual(helper[name](), { done: true, value: undefined });
        }
        return { done: true };
      } };
      helper = method(method === joint.zip ? [source] : { source });
      if (started) helper.next();
      assert.throws(() => helper.return.call({}), TypeError);
      assert.deepEqual(helper.return(), { done: true, value: undefined });
      assert.equal(closes, 1);
      assert.deepEqual(helper.next(), { done: true, value: undefined });
      assert.deepEqual(helper.return(), { done: true, value: undefined });
    }
  }
});
