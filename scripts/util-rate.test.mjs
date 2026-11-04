import assert from "node:assert/strict";
import Module, { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as wait } from "node:timers/promises";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const util = require("node:util");
const rate = fileURLToPath(new URL("../runtime/util/rate.cjs", import.meta.url));
const debounceFile = fileURLToPath(new URL("../runtime/util/debounce.cjs", import.meta.url));
// Node 24.22+ and 26.10+ ship both helpers. Set them aside so every test runs the
// backfill on whatever Node runs the suite; the native pair, when present, is the
// oracle the backfill's errors are compared against.
const native = "debounce" in util ? { debounce: util.debounce, throttle: util.throttle } : null;
delete util.debounce;
delete util.throttle;
const hideNative = 'const util = require("node:util"); delete util.debounce; delete util.throttle;';
const { installUtilRateShims, resolveUtilRateFacade } = require(rate);
installUtilRateShims();

test("installs lazy getters shaped like Node's, loading nothing until read", () => {
  for (const name of ["debounce", "throttle"]) {
    const descriptor = Object.getOwnPropertyDescriptor(util, name);
    assert.equal(typeof descriptor.get, "function", `${name} is a getter`);
    assert.equal(descriptor.set, undefined, `${name} has no setter`);
    assert.equal(descriptor.enumerable, true);
    assert.equal(descriptor.configurable, true);
  }
  assert.equal(require.cache[debounceFile], undefined, "debounce.cjs loaded before its first read");
  assert.equal(typeof util.debounce, "function");
  assert.equal(util.debounce, util.debounce);
  assert.equal(util, require("util"));
});

test("routes ESM imports of util through a facade that re-exports the singleton", async () => {
  const facade = resolveUtilRateFacade("node:util", { conditions: ["node", "import"] }, true);
  assert.equal(facade.url, resolveUtilRateFacade("util", { conditions: ["import"] }, true).url);
  assert.equal(resolveUtilRateFacade("node:util", { conditions: ["import"], parentURL: facade.url }, true), null);
  assert.equal(resolveUtilRateFacade("node:util", { conditions: ["node", "require"] }, true), null);
  // Node 22.15's registerHooks passes a CommonJS require's conditions as a Set.
  assert.equal(resolveUtilRateFacade("util", { conditions: new Set(["require", "node"]) }, true), null);
  assert.equal(resolveUtilRateFacade("node:util", { conditions: ["import"] }, false), null);
  const namespace = await import(facade.url);
  assert.equal(namespace.default, util);
  assert.equal(namespace.debounce, util.debounce);
  assert.equal(namespace.throttle, util.throttle);
  assert.equal(namespace.promisify, util.promisify);
});

test("preload exposes named ESM imports on both loader tiers", () => {
  const dir = mkdtempSync(join(tmpdir(), "util-rate-"));
  const hide = join(dir, "hide-native.cjs");
  writeFileSync(hide, hideNative);
  const preload = fileURLToPath(new URL("../runtime/preload.cjs", import.meta.url));
  const source = `
    import { debounce, throttle, promisify } from "node:util";
    import { createRequire } from "node:module";
    const require = createRequire(import.meta.url);
    const backfilled = require("node:util").debounce === debounce && ${JSON.stringify(debounceFile)} in require.cache;
    if (!backfilled || [throttle, promisify].some((value) => typeof value !== "function")) process.exit(1);
  `;
  // Without sync `registerHooks` (before 22.15) the async tier is the only one,
  // and the flag that forces it elsewhere may not exist.
  const tiers = typeof Module.registerHooks === "function" ? [[], ["--no-experimental-require-module"]] : [[]];
  for (const flags of tiers) {
    const result = spawnSync(process.execPath, [
      ...flags, "--require", hide, "--require", preload, "--input-type=module", "-e", source,
    ], {
      env: { ...process.env, __NUB_VERSION: "0.1.0" },
      encoding: "utf8",
    });
    assert.equal(result.status, 0, result.stderr);
  }
});

test("debounce shares the latest result and exposes window controls", async () => {
  const calls = [];
  const fn = util.debounce(function (value) {
    assert.equal(this, fn);
    calls.push(value);
    return value;
  }, 100);
  const first = fn(1);
  const second = fn(2);
  assert.equal(fn.pending, second);
  assert.equal(fn.pendingCount, 2);
  assert.equal(fn.unref(), fn);
  assert.equal(fn.ref(), fn);
  fn.flush();
  assert.deepEqual(await Promise.all([first, second]), [2, 2]);
  assert.deepEqual(calls, [2]);
  assert.equal(fn.pending, null);
  assert.equal(fn.pendingCount, 0);
});

test("debounce leading, supersession, cancellation, and signal", async () => {
  const values = [];
  const fn = util.debounce((value) => { values.push(value); return value; }, 100, {
    leading: true,
    rejectOnCancel: true,
  });
  assert.equal(await fn(1), 1);
  const superseded = fn(2);
  const current = fn(3);
  await assert.rejects(superseded, { code: "ABORT_ERR" });
  fn.flush();
  assert.equal(await current, 3);
  assert.deepEqual(values, [1, 3]);

  assert.equal(await fn(4), 4);
  const pending = fn(5);
  const cause = new Error("stop");
  fn.cancel(cause);
  await assert.rejects(pending, (error) => error.code === "ABORT_ERR" && error.cause === cause);

  const controller = new AbortController();
  const signaled = util.debounce(() => 1, 100, { signal: controller.signal });
  const aborted = signaled();
  controller.abort(cause);
  await assert.rejects(aborted, (error) => error.code === "ABORT_ERR" && error.cause === cause);
  await assert.rejects(signaled(), { code: "ABORT_ERR" });
});

test("throttle preserves queue order and limits concurrent invocations", async () => {
  const starts = [];
  const releases = [];
  const fn = util.throttle((value) => {
    starts.push(value);
    return new Promise((resolve) => releases.push(() => resolve(value)));
  }, 3, 100, { concurrency: 1 });
  const results = [fn(1), fn(2), fn(3)];
  assert.deepEqual(starts, [1]);
  assert.equal(fn.activeCount, 1);
  assert.equal(fn.pendingCount, 2);
  assert.equal(fn.pending, results[2]);
  assert.equal(fn.hasImmediateCapacity(), false);
  releases.shift()();
  await wait(0);
  assert.deepEqual(starts, [1, 2]);
  releases.shift()();
  await wait(0);
  assert.deepEqual(starts, [1, 2, 3]);
  releases.shift()();
  assert.deepEqual(await Promise.all(results), [1, 2, 3]);
  await wait(0);
  assert.equal(fn.activeCount, 0);
});

test("throttle handles overflow, cancellation, strict windows, and signal", async () => {
  const fn = util.throttle((value) => value, 1, 100, { maxPending: 1, strict: true });
  assert.equal(await fn(1), 1);
  const queued = fn(2);
  const dropped = fn(3);
  await assert.rejects(dropped, { code: "ERR_THROTTLED" });
  assert.equal(fn.pendingCount, 1);
  fn.cancel("reset");
  await assert.rejects(queued, (error) => error.code === "ABORT_ERR" && error.cause === "reset");
  assert.equal(await fn(4), 4);

  const drop = util.throttle((value) => value, 1, 100, { overflow: "drop" });
  assert.equal(await drop(1), 1);
  await assert.rejects(drop(2), { code: "ERR_THROTTLED" });

  const controller = new AbortController();
  const signaled = util.throttle((value) => value, 1, 100, { signal: controller.signal });
  assert.equal(await signaled(1), 1);
  const pending = signaled(2);
  controller.abort("stop");
  await assert.rejects(pending, (error) => error.code === "ABORT_ERR" && error.cause === "stop");
  await assert.rejects(signaled(3), { code: "ABORT_ERR" });
});

test("errors carry Node's codes, messages, and shape", async () => {
  const misuse = [
    (impl) => impl.debounce(null, 1),
    (impl) => impl.debounce(() => {}, 1.5),
    (impl) => impl.debounce(() => {}, 2 ** 31),
    (impl) => impl.debounce(() => {}, 1, []),
    (impl) => impl.debounce(() => {}, 1, { signal: null }),
    (impl) => impl.throttle(() => {}, 0, 1),
    (impl) => impl.throttle(() => {}, 1, 1, { overflow: "other" }),
    (impl) => impl.throttle(() => {}, 1, 1, { strict: 1 }),
  ];
  const describe = (error) => ({
    constructor: error.constructor,
    name: error.name,
    code: error.code,
    message: error.message,
    string: String(error),
    header: error.stack.split("\n")[0],
    keys: Object.keys(error),
  });
  const thrown = (impl, call) => {
    try { call(impl); } catch (error) { return describe(error); }
    assert.fail("expected a throw");
  };
  const dropped = async (impl) => {
    const fn = impl.throttle(() => {}, 1, 100, { overflow: "drop" });
    fn();
    const error = await fn().then(() => assert.fail("expected a drop"), (e) => e);
    return { ...describe(error), header: undefined };
  };

  assert.deepEqual(thrown(util, misuse[0]), {
    constructor: TypeError,
    name: "TypeError",
    code: "ERR_INVALID_ARG_TYPE",
    message: 'The "fn" argument must be of type function. Received null',
    string: 'TypeError [ERR_INVALID_ARG_TYPE]: The "fn" argument must be of type function. Received null',
    header: 'TypeError [ERR_INVALID_ARG_TYPE]: The "fn" argument must be of type function. Received null',
    keys: ["code"],
  });
  assert.equal(thrown(util, misuse[5]).message,
    'The value of "limit" is out of range. It must be >= 1 && <= 9007199254740991. Received 0');
  assert.equal(thrown(util, misuse[6]).message,
    "The property 'options.overflow' must be one of: 'queue', 'drop'. Received 'other'");
  assert.equal((await dropped(util)).string, "Error [ERR_THROTTLED]: The call was dropped by the throttled function");
  if (native) {
    for (const call of misuse) assert.deepEqual(thrown(util, call), thrown(native, call), call.toString());
    assert.deepEqual(await dropped(util), await dropped(native));
  }
});

test("abort cannot be suppressed by an earlier signal listener", async () => {
  for (const create of [
    (signal) => util.debounce(() => 1, 1000, { signal }),
    (signal) => util.throttle(() => 1, 1, 1000, { signal }),
  ]) {
    const controller = new AbortController();
    controller.signal.addEventListener("abort", (event) => event.stopImmediatePropagation());
    const fn = create(controller.signal);
    const first = fn();
    if (fn.activeCount !== undefined) await first;
    const pending = fn.activeCount !== undefined ? fn() : first;
    controller.abort("stopped");
    await assert.rejects(
      Promise.race([pending, wait(100).then(() => { throw new Error("abort did not settle the call"); })]),
      { code: "ABORT_ERR" },
    );
  }
});

test("shim operations retain captured intrinsics after global mutation", () => {
  const source = `
    ${hideNative}
    require(${JSON.stringify(rate)}).installUtilRateShims();
    const NativeThen = Promise.prototype.then;
    Array.prototype.push = () => { throw new Error("mutated push"); };
    Array.prototype.slice = () => { throw new Error("mutated slice"); };
    Promise.resolve = () => { throw new Error("mutated resolve"); };
    Promise.reject = () => { throw new Error("mutated reject"); };
    Number.isInteger = () => false;
    globalThis.Promise = class {};
    process.hrtime = () => { throw new Error("mutated clock"); };
    const debounced = util.debounce(value => value, 1);
    const throttled = util.throttle(value => value, 1, 1);
    const values = [];
    const onError = error => { process.stderr.write(String(error)); process.exitCode = 1; };
    NativeThen.call(debounced(1), value => { values[0] = value; if (values.length === 2) process.stdout.write(values.join(",")); }, onError);
    NativeThen.call(throttled(2), value => { values[1] = value; if (values[0] !== undefined) process.stdout.write(values.join(",")); }, onError);
  `;
  const result = spawnSync(process.execPath, ["-e", source], { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, "1,2");
});
