import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { setTimeout as wait } from "node:timers/promises";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { installUtilRateShims, resolveUtilRateFacade } = require("../runtime/util/rate.cjs");
installUtilRateShims();
const util = require("node:util");

test("preload exposes named ESM imports on both loader tiers", () => {
  const preload = fileURLToPath(new URL("../runtime/preload.cjs", import.meta.url));
  const source = 'import { debounce, throttle, promisify } from "node:util"; if ([debounce, throttle, promisify].some((value) => typeof value !== "function")) process.exit(1)';
  for (const flags of [[], ["--no-experimental-require-module"]]) {
    const result = spawnSync(process.execPath, [
      ...flags, "--require", preload, "--input-type=module", "-e", source,
    ], {
      env: { ...process.env, __NUB_VERSION: "0.1.0" },
      encoding: "utf8",
    });
    assert.equal(result.status, 0, result.stderr);
  }
});

test("installs both shims on the native util singleton", async () => {
  assert.equal(util, require("util"));
  assert.equal(typeof util.debounce, "function");
  assert.equal(typeof util.throttle, "function");
  const original = util.debounce;
  installUtilRateShims();
  assert.equal(util.debounce, original);

  const facade = resolveUtilRateFacade("node:util", { conditions: ["node", "import"] }, true);
  if (facade) {
    assert.equal(facade.url, resolveUtilRateFacade("util", { conditions: ["import"] }, true).url);
    assert.equal(resolveUtilRateFacade("node:util", { conditions: ["import"], parentURL: facade.url }, true), null);
    assert.equal(resolveUtilRateFacade("node:util", { conditions: ["node", "require"] }, true), null);
    assert.equal(resolveUtilRateFacade("node:util", { conditions: ["import"] }, false), null);
    const namespace = await import(facade.url);
    assert.equal(namespace.default, util);
    assert.equal(namespace.debounce, util.debounce);
    assert.equal(namespace.throttle, util.throttle);
    assert.equal(namespace.promisify, util.promisify);
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

test("Node argument error codes are preserved", () => {
  assert.throws(() => util.debounce(null, 1), { code: "ERR_INVALID_ARG_TYPE" });
  assert.throws(() => util.debounce(() => {}, -1), { code: "ERR_OUT_OF_RANGE" });
  assert.throws(() => util.debounce(() => {}, 1, []), { code: "ERR_INVALID_ARG_TYPE" });
  assert.throws(() => util.throttle(() => {}, 0, 1), { code: "ERR_OUT_OF_RANGE" });
  assert.throws(() => util.throttle(() => {}, 1, 1, { overflow: "other" }), { code: "ERR_INVALID_ARG_VALUE" });
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
  const rate = fileURLToPath(new URL("../runtime/util/rate.cjs", import.meta.url));
  const source = `
    const util = require("node:util");
    require(${JSON.stringify(rate)}).installUtilRateShims();
    const NativeThen = Promise.prototype.then;
    Array.prototype.push = () => { throw new Error("mutated push"); };
    Array.prototype.slice = () => { throw new Error("mutated slice"); };
    Promise.resolve = () => { throw new Error("mutated resolve"); };
    Promise.reject = () => { throw new Error("mutated reject"); };
    Number.isInteger = () => false;
    globalThis.Promise = class {};
    globalThis.performance = { now: () => { throw new Error("mutated clock"); } };
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
