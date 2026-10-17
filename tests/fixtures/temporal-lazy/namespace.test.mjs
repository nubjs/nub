import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import test from "node:test";

function probe(source) {
  const result = spawnSync(process.execPath, ["--input-type=commonjs", "--eval", `
    const assert = require("node:assert/strict");
    const { createRequire } = require("node:module");
    const common = require("./runtime/preload-common.cjs");
    delete globalThis.Temporal;
    delete Date.prototype.toTemporalInstant;
    ${source}
  `], { cwd: new URL("../../../", import.meta.url), encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr || result.error?.message);
}

const checkNamespace = `
  const T = globalThis.Temporal;
  const members = [
    "Now", "Instant", "PlainDate", "PlainTime", "PlainDateTime",
    "ZonedDateTime", "PlainYearMonth", "PlainMonthDay", "Duration",
  ];
  assert.equal(Object.getPrototypeOf(T), Object.prototype);
  assert.equal(Object.isExtensible(T), true);
  assert.equal(Object.isFrozen(T), false);
  assert.deepEqual(Object.getOwnPropertyNames(T).sort(), members.sort());
  assert.deepEqual(Object.getOwnPropertySymbols(T), [Symbol.toStringTag]);
  for (const name of members) {
    assert.deepEqual(Object.getOwnPropertyDescriptor(T, name), {
      value: polyfill.Temporal[name], writable: true, enumerable: false, configurable: true,
    }, name);
  }
  assert.deepEqual(Object.getOwnPropertyDescriptor(T, Symbol.toStringTag), {
    value: "Temporal", writable: false, enumerable: false, configurable: true,
  });
  assert.equal(Object.prototype.toString.call(T), "[object Temporal]");
  assert.equal(Date.prototype.toTemporalInstant, polyfill.toTemporalInstant);
  assert.ok(new Date(0).toTemporalInstant() instanceof T.Instant);
  assert.ok(polyfill.Temporal.PlainDate.from("2026-10-17") instanceof T.PlainDate);

  const copy = Object.defineProperties({}, Object.getOwnPropertyDescriptors(T));
  const now = {};
  Object.defineProperty(copy, "Now", { value: now, writable: true, configurable: true });
  assert.equal(copy.Now, now);
  assert.equal(T.Now, polyfill.Temporal.Now);
`;

test("Temporal installation normalizes the namespace without changing polyfill members", () => {
  probe(`
    const polyfill = require("@js-temporal/polyfill");
    const original = Object.getOwnPropertyDescriptors(polyfill.Temporal);
    common.installTemporalGlobal(polyfill);
    ${checkNamespace}
    assert.deepEqual(Object.getOwnPropertyDescriptors(polyfill.Temporal), original);
  `);
});

test("lazy Temporal defers resolution and loading until first access", () => {
  probe(`
    const req = createRequire(require("node:path").resolve("runtime/preload-common.cjs"));
    let resolves = 0;
    let loads = 0;
    const lazyRequire = (path) => { loads++; return req(path); };
    lazyRequire.resolve = (name) => { resolves++; return req.resolve(name); };
    RegExp.$_ = "temporal-lazy-sentinel";
    common.installTemporalLazyGlobal(lazyRequire);
    assert.equal(RegExp.$_, "temporal-lazy-sentinel");
    assert.equal(resolves, 0);
    assert.equal(loads, 0);
    assert.equal(typeof Object.getOwnPropertyDescriptor(globalThis, "Temporal").get, "function");
    const first = globalThis.Temporal;
    assert.equal(globalThis.Temporal, first);
    assert.equal(resolves, 1);
    assert.equal(loads, 1);
    const polyfill = req("@js-temporal/polyfill");
    ${checkNamespace}
  `);
});

test("compiled Temporal uses the same namespace attributes", {
  skip: !process.features.require_module && "the unbundled bridge needs native require(esm)",
}, () => {
  probe(`
    const { installCompiledTemporalLazyGlobal } = require("./runtime/compile-lazy-temporal.cjs");
    installCompiledTemporalLazyGlobal();
    assert.equal(typeof Object.getOwnPropertyDescriptor(globalThis, "Temporal").get, "function");
    const first = globalThis.Temporal;
    assert.equal(globalThis.Temporal, first);
    const polyfill = require("./runtime/compile-lazy-temporal.mjs");
    ${checkNamespace}
  `);
});

test("Temporal installers preserve an existing native namespace and Date method", () => {
  probe(`
    const native = {};
    const nativeInstant = () => {};
    globalThis.Temporal = native;
    Date.prototype.toTemporalInstant = nativeInstant;
    const unused = () => { throw new Error("polyfill must not load"); };
    unused.resolve = unused;
    assert.equal(common.installTemporalGlobal({}), native);
    common.installTemporalLazyGlobal(unused);
    require("./runtime/compile-lazy-temporal.cjs").installCompiledTemporalLazyGlobal();
    assert.equal(globalThis.Temporal, native);
    assert.equal(Date.prototype.toTemporalInstant, nativeInstant);
    delete globalThis.Temporal;
    common.installTemporalGlobal(require("@js-temporal/polyfill"));
    assert.equal(Date.prototype.toTemporalInstant, nativeInstant);
  `);
});

test("assigning the lazy Temporal global does not load the polyfill", () => {
  probe(`
    const unused = () => { throw new Error("polyfill must not load"); };
    unused.resolve = unused;
    common.installTemporalLazyGlobal(unused);
    const replacement = {};
    globalThis.Temporal = replacement;
    assert.equal(globalThis.Temporal, replacement);
  `);
});
