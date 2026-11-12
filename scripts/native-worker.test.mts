import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import test, { describe } from "node:test";

const exec = promisify(execFile);
const runtime = fileURLToPath(new URL("../runtime/", import.meta.url));
const nativeOptions = {
  skip: !process.allowedNodeEnvironmentFlags.has("--experimental-web-worker"),
  concurrency: true,
};
type CompiledBootstrap = {
  createRequire: typeof import("node:module").createRequire;
  getBuiltin: typeof process.getBuiltinModule;
  needsWorker: boolean;
  needsChildProcess: boolean;
};

async function run<Arg = undefined>(
  fn: (runtime: string, arg: Arg) => unknown, native = false, arg?: Arg,
): Promise<void> {
  const source = `(${fn.toString()})(${JSON.stringify(runtime)}, ${JSON.stringify(arg)})`;
  const { stderr } = await exec(process.execPath, [
    ...(native ? ["--experimental-web-worker"] : []), "-e", source,
  ], { timeout: 10000, maxBuffer: 1024 * 1024 });
  assert.equal(stderr, "");
}

void describe("native Worker handoff", { concurrency: true }, () => {
  for (const preload of ["preload.cjs", "worker-polyfill.mjs"]) {
    void test(`${preload} preserves native Worker and Blob identities`, nativeOptions, async () => {
      await run((runtime, preload) => {
        const assert: typeof import("node:assert/strict") = require("node:assert/strict");
        // oxlint-disable-next-line typescript/unbound-method -- Compare method identities without calling them.
        const before = [Worker, Blob, URL.createObjectURL, URL.revokeObjectURL];
        require(runtime + preload);
        // oxlint-disable-next-line typescript/unbound-method -- Compare method identities without calling them.
        assert.deepEqual([Worker, Blob, URL.createObjectURL, URL.revokeObjectURL], before);
        const registry = require(runtime + "worker-blob-url.cjs");
        const url = URL.createObjectURL(new Blob(["postMessage(42)"], { type: "text/javascript" }));
        assert.equal(registry.blobUrlSources.size, 0);
        const worker = new Worker(url);
        worker.onerror = e => { throw e.error; };
        worker.onmessage = e => { assert.equal(e.data, 42); worker.terminate(); URL.revokeObjectURL(url); };
      }, true, preload);
    });
  }

  for (const needsWorker of [false, true]) {
    void test(`compiled preamble preserves native Worker (needsWorker=${needsWorker})`, nativeOptions, async () => {
      await run(async (runtime, needsWorker) => {
        const assert: typeof import("node:assert/strict") = require("node:assert/strict");
        // oxlint-disable-next-line typescript/unbound-method -- Compare method identities without calling them.
        const before = [Worker, Blob, URL.createObjectURL];
        (process as NodeJS.Process & { [key: symbol]: CompiledBootstrap })[Symbol.for("nub.compile.bootstrap")] = {
          createRequire: require("node:module").createRequire,
          getBuiltin: process.getBuiltinModule.bind(process),
          needsWorker, needsChildProcess: false,
        };
        await import(require("node:url").pathToFileURL(runtime + "compile-preamble.mjs"));
        // oxlint-disable-next-line typescript/unbound-method -- Compare method identities without calling them.
        assert.deepEqual([Worker, Blob, URL.createObjectURL], before);
      }, true, needsWorker);
    });
  }

  for (const preload of ["preload.cjs", "worker-polyfill.mjs"]) {
    void test(`${preload} keeps native scope descriptors, functions and message delivery`, nativeOptions, async () => {
      await run((runtime, preload) => {
        const assert: typeof import("node:assert/strict") = require("node:assert/strict");
        const body = (runtime: string, preload: string) => {
          const require = process.getBuiltinModule("module").createRequire(runtime + "worker-polyfill.mjs");
          const assert: typeof import("node:assert/strict") = require("node:assert/strict");
          const keys = ["self", "name", "addEventListener", "removeEventListener", "dispatchEvent", "postMessage", "close", "onmessage", "onmessageerror", "importScripts"];
          const before = keys.map(key => [Reflect.get(globalThis, key), Object.getOwnPropertyDescriptor(globalThis, key)]);
          require(runtime + preload);
          assert.deepEqual(keys.map(key => [Reflect.get(globalThis, key), Object.getOwnPropertyDescriptor(globalThis, key)]), before);
          addEventListener("message", event => postMessage({ value: event.data + 1, name }), { once: true });
        };
        const source = `(${body.toString()})(${JSON.stringify(runtime)}, ${JSON.stringify(preload)})`;
        const worker = new Worker("data:text/javascript," + encodeURIComponent(source), { name: "native" });
        worker.onerror = e => { throw e.error; };
        worker.onmessage = e => { assert.deepEqual(e.data, { value: 42, name: "native" }); worker.terminate(); };
        worker.postMessage(41);
      }, true, preload);
    });
  }

  void test("ordinary worker_threads receives the scope shim with the native flag", nativeOptions, async () => {
    await run(runtime => {
      const assert: typeof import("node:assert/strict") = require("node:assert/strict");
      const { Worker: Thread }: typeof import("node:worker_threads") = require("node:worker_threads");
      const body = (runtime: string) => {
        require(runtime + "worker-polyfill.mjs");
        addEventListener("message", e => postMessage(e.data + 1), { once: true });
      };
      const worker = new Thread(`(${body.toString()})(${JSON.stringify(runtime)})`, { eval: true });
      worker.on("error", error => { throw error; });
      worker.once("message", (value: number) => { assert.equal(value, 42); void worker.terminate(); });
      worker.postMessage(41);
    }, true);
  });

  void test("compiled preamble retains the lazy fallback Worker", { concurrency: true }, async () => {
    await run(async runtime => {
      const assert: typeof import("node:assert/strict") = require("node:assert/strict");
      assert.equal(typeof Worker, "undefined");
      (process as NodeJS.Process & { [key: symbol]: CompiledBootstrap })[Symbol.for("nub.compile.bootstrap")] = {
        createRequire: require("node:module").createRequire,
        getBuiltin: process.getBuiltinModule.bind(process),
        needsWorker: false, needsChildProcess: false,
      };
      await import(require("node:url").pathToFileURL(runtime + "compile-preamble.mjs"));
      assert.equal(typeof Object.getOwnPropertyDescriptor(globalThis, "Worker")?.get, "function");
      assert.equal(typeof (globalThis as unknown as Record<string, unknown>)["Work" + "er"], "function");
      assert.equal(Object.getOwnPropertyDescriptor(globalThis, "Worker")?.value, Worker);
    });
  });

  void test("default preload retains fallback Blob workers", { concurrency: true }, async () => {
    await run(runtime => {
      const assert: typeof import("node:assert/strict") = require("node:assert/strict");
      assert.equal(typeof Worker, "undefined");
      process.env.NODE_OPTIONS = "--require=" + JSON.stringify(runtime + "preload.cjs");
      require(runtime + "preload.cjs");
      const registry = require(runtime + "worker-blob-url.cjs");
      const url = URL.createObjectURL(new Blob(["postMessage(42)"], { type: "text/javascript" }));
      assert.equal(registry.blobUrlSources.size, 1);
      const worker = new Worker(url);
      worker.onerror = e => { throw e.error; };
      worker.onmessage = e => { assert.equal(e.data, 42); worker.terminate(); URL.revokeObjectURL(url); };
    });
  });

  void test("fallback ErrorEvent exposes its fields as read-only prototype accessors", { concurrency: true }, async () => {
    await run(runtime => {
      const assert: typeof import("node:assert/strict") = require("node:assert/strict");
      const g = globalThis as unknown as Record<string, unknown>;
      const native = g.ErrorEvent as { prototype: object } | undefined;
      delete g.Worker; delete g.ErrorEvent;
      require(runtime + "worker-polyfill.mjs");
      const fields = ["message", "filename", "lineno", "colno", "error"];
      const shape = (proto: object) => fields.map(key => [key, Object.entries(Object.getOwnPropertyDescriptor(proto, key) ?? {})
        .map(([name, value]) => [name, typeof value === "function" ? "function" : value])]);
      const accessor = [["get", "function"], ["set", undefined], ["enumerable", true], ["configurable", true]];
      const expected = fields.map(key => [key, accessor]);
      if (native) assert.deepEqual(shape(native.prototype), expected);
      const WorkerCtor = g.Worker as new (src: string, opts: object) => EventTarget & { terminate(): Promise<number> };
      const worker = new WorkerCtor("throw new Error('shape failure')", { eval: true, execArgv: [] });
      worker.addEventListener("error", (raw: Event) => {
        const event = raw as Event & Record<string, unknown>;
        event.preventDefault();
        assert.equal(event instanceof Event, true);
        assert.equal(event.type, "error");
        assert.deepEqual(shape(Object.getPrototypeOf(event)), expected);
        assert.deepEqual(Reflect.ownKeys(event), Reflect.ownKeys(new Event("error")));
        assert.match(event.message as string, /shape failure/);
        assert.equal((event.error as Error).message, "shape failure");
        assert.equal(typeof event.filename, "string");
        assert.equal(typeof event.lineno, "number");
        assert.equal(typeof event.colno, "number");
        assert.throws(() => { "use strict"; event.message = "changed"; }, TypeError);
        const Ctor = event.constructor as new (type: string, init?: object) => Record<string, unknown>;
        const blank = new Ctor("error");
        assert.deepEqual(fields.map(key => blank[key]), ["", "", 0, 0, undefined]);
        assert.equal(new Ctor("error", { error: null }).error, null);
        assert.equal(Object.prototype.toString.call(event), "[object ErrorEvent]");
        void worker.terminate();
      }, { once: true });
    });
  });

});
