import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { describe, test } from "node:test";

const exec = promisify(execFile);
const runtime = fileURLToPath(new URL("../runtime/", import.meta.url));
// Poison only inside a child process, restore before assertions or async work.
// Loading the snapshot first reproduces preload followed by application mutation,
// including helpers whose first require happens after the mutation.
type PoisonEntry = readonly [target: object, key: PropertyKey];
declare function poisoned<T>(entries: readonly PoisonEntry[], work: () => T): T;
type WatchHooks = { reportDep(path: string): void };
type PnpApi = { resolveRequest(specifier: string, issuer: string, options: { conditions: Set<string> }): string };

async function run(fn: () => void): Promise<void> {
  const source = `
    const assert = require("node:assert/strict");
    const runtime = ${JSON.stringify(runtime)};
    require(runtime + "primordials.cjs");
    const define = Object.defineProperty;
    const descriptor = Object.getOwnPropertyDescriptor;
    function poisoned(entries, work) {
      const saved = entries.map(([target, key]) => [target, key, descriptor(target, key)]);
      const fail = () => { throw new Error("poisoned builtin was called"); };
      for (const [target, key] of entries) define(target, key, { value: fail, configurable: true, writable: true });
      try { return work(); }
      finally { for (const [target, key, value] of saved) define(target, key, value); }
    }
    (${fn.toString()})();
  `;
  const { stderr } = await exec(process.execPath, ["-e", source], { timeout: 10000 });
  assert.equal(stderr, "");
}

void describe("CJS runtime captured builtins", { concurrency: true }, () => {
  void test("lazy property setup and first read survive a replaced defineProperty", async () => {
    await run(() => {
      const target: { value?: number; assigned?: number } = {};
      let calls = 0;
      poisoned([[Object, "defineProperty"]], () => {
        const { defineLazy } = require(runtime + "compile-lazy.cjs");
        defineLazy(target, "value", () => ++calls);
        if (target.value !== 1) throw new Error("wrong lazy value");
        defineLazy(target, "assigned", () => ++calls);
        target.assigned = 42;
      });
      assert.equal(target.value, 1);
      assert.equal(target.assigned, 42);
      assert.equal(calls, 1);
      assert.deepEqual(Object.getOwnPropertyDescriptor(target, "value"), {
        value: 1, writable: true, enumerable: true, configurable: true,
      });
    });
  });

  void test("preload marker still installs after Object.defineProperty is replaced", async () => {
    await run(() => {
      const common = require(runtime + "preload-common.cjs");
      process.env.__NUB_VERSION = "primordial-test";
      poisoned([[Object, "defineProperty"]], () => common.installVersionMarker());
      assert.equal(process.versions.nub, "primordial-test");
    });
  });

  void test("watch reporting uses captured private Set and queue methods", async () => {
    await run(() => {
      const common = require(runtime + "preload-common.cjs");
      process.env.WATCH_REPORT_DEPENDENCIES = "1";
      let sent: unknown;
      let flush!: () => void;
      let hooks!: WatchHooks;
      (process as unknown as { send(message: unknown): void }).send = value => { sent = value; };
      (globalThis as unknown as { setImmediate(callback: () => void): void }).setImmediate = callback => { flush = callback; };
      poisoned([[globalThis, "Set"], [Set.prototype, "has"], [Set.prototype, "add"],
        [Array.prototype, "push"], [Array.prototype, "splice"]], () => {
        common.installWatchReporting({ setWatchHooks(value: WatchHooks) { hooks = value; } });
        hooks.reportDep("one.js");
        hooks.reportDep("one.js");
        hooks.reportDep("two.js");
        flush();
      });
      assert.deepEqual(sent, { "watch:require": ["one.js", "two.js"] });
    });
  });

  void test("Blob capture survives replaced reflection and collection methods", async () => {
    await run(() => {
      const registry = require(runtime + "worker-blob-url.cjs");
      registry.installBlobUrlSupport();
      let source, revoked;
      poisoned([[Reflect, "construct"], [WeakMap.prototype, "get"], [WeakMap.prototype, "set"],
        [Map.prototype, "get"], [Map.prototype, "set"], [Map.prototype, "delete"],
        [Array.prototype, "push"], [Buffer, "from"], [Buffer, "concat"],
        [Buffer, "isBuffer"], [Buffer.prototype, "toString"]], () => {
        const nested = new Blob(["hello"]);
        const url = URL.createObjectURL(new Blob([nested, " world"]));
        source = registry.blobUrlSource(url);
        URL.revokeObjectURL(url);
        revoked = registry.blobUrlSource(url);
      });
      assert.equal(source, "hello world");
      assert.equal(revoked, undefined);
      assert.equal(registry.blobUrlSources.size, 0);
    });
  });

  void test("Blob URL wrappers retain the original receiver after global replacement", async () => {
    await run(() => {
      const NativeURL = URL;
      const nativeCreate = URL.createObjectURL.bind(URL);
      const nativeRevoke = URL.revokeObjectURL.bind(URL);
      URL.createObjectURL = function (this: typeof URL, blob) {
        if (this !== NativeURL) throw new Error("wrong create receiver");
        return nativeCreate(blob);
      };
      URL.revokeObjectURL = function (this: typeof URL, url) {
        if (this !== NativeURL) throw new Error("wrong revoke receiver");
        return nativeRevoke(url);
      };
      const registry = require(runtime + "worker-blob-url.cjs");
      registry.installBlobUrlSupport();
      let source;
      poisoned([[globalThis, "URL"]], () => {
        const url = NativeURL.createObjectURL(new Blob(["receiver"]));
        source = registry.blobUrlSource(url);
        NativeURL.revokeObjectURL(url);
      });
      assert.equal(source, "receiver");
      assert.equal(registry.blobUrlSources.size, 0);
    });
  });

  void test("late PnP helper preserves package type and caller conditions", async () => {
    await run(() => {
      const fs = require("node:fs"), path = require("node:path"), os = require("node:os");
      const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nub-primordials-"));
      try {
        fs.writeFileSync(path.join(dir, "package.json"), '{"type":"module"}');
        let actualConditions!: Set<string>;
        let result!: { format: string };
        const api: PnpApi = { resolveRequest(specifier, issuer, { conditions }) {
          actualConditions = conditions;
          return path.join(dir, "entry.js");
        } };
        poisoned([[JSON, "parse"], [globalThis, "Set"], [Set.prototype, "add"],
          [String.prototype, "endsWith"]], () => {
          const { pnpResolveEsm } = require(runtime + "pnp-util.cjs");
          result = pnpResolveEsm(api, "example", { conditions: ["node", "custom"] });
        });
        assert.equal(result.format, "module");
        assert.deepEqual([...actualConditions], ["node", "custom"]);
      } finally { fs.rmSync(dir, { recursive: true, force: true }); }
    });
  });

  void test("util facade uses captured array detection and includes", async () => {
    await run(() => {
      const util = require("node:util");
      delete util.debounce;
      delete util.throttle;
      const { resolveUtilRateFacade } = require(runtime + "util/rate.cjs");
      const [esm, cjs]: [{ shortCircuit: boolean } | null, unknown] = poisoned(
        [[Array, "isArray"], [Array.prototype, "includes"]],
        () => [
          resolveUtilRateFacade("node:util", { conditions: ["import"] }, true),
          resolveUtilRateFacade("node:util", { conditions: ["require"] }, true),
        ],
      );
      assert.equal(esm?.shortCircuit, true);
      assert.equal(cjs, null);
    });
  });
});
