// POSITIVE fixture — every surface @nubjs/types adds over @types/node must resolve
// under the canonical consumer config: lib es2024 (no dom) + types ["node","@nubjs/types"].
// Expected: tsc --noEmit exits 0.

// Data-format import wildcards (declare module "*.yaml" / "*.toml" / …).
import yamlCfg from "./config.yaml";
import tomlCfg from "./config.toml";

// Browser-shape Worker global + its methods/handlers.
const worker = new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
worker.postMessage({ yaml: yamlCfg, toml: tomlCfg });
worker.onmessage = (ev) => console.log(ev.data);
worker.onerror = (ev) => console.error(ev);

// Node worker_threads compatibility: EventEmitter methods (node-channel shapes),
// the online/exit lifecycle, an awaited terminate(), and { eval: true }. The
// callbacks are UNannotated so the per-event payload type is INFERRED from the
// overload, then pinned under a constraint — an overload-shape regression
// (e.g. exit→string) fails this fixture instead of falling through to the
// generic listener overload.
worker
  .on("message", (value) => console.log(value)) // raw value, inferred `any`
  .on("error", (err) => {
    const e: Error = err; // node-channel error is a bare Error
    void e;
  })
  .on("exit", (code) => {
    const n: number = code; // exit code is numeric
    void n;
  })
  .on("online", () => console.log("online"));
worker.once("message", (value) => console.log(value));
worker.off("message", () => {});
const emitted: boolean = worker.emit("message", 1);
void emitted;
const exitCode: Promise<number> = worker.terminate();
void exitCode;
const inlineWorker = new Worker("self.postMessage(1)", { eval: true });
void inlineWorker;

// reportError (WinterTC global; not in @types/node).
reportError(new Error("boom"));

// Promise.allKeyed / Promise.allSettledKeyed (TC39 await dictionary). The result
// properties are pinned to concrete types so a mapped-type regression — losing
// `Awaited`, or widening a value to `any` — fails this fixture instead of
// silently typechecking.
const dict = await Promise.allKeyed({ shape: Promise.resolve("square"), mass: 12 });
const dictShape: string = dict.shape;
const dictMass: number = dict.mass;
void dictShape;
void dictMass;
const settledDict = await Promise.allSettledKeyed({ ok: Promise.resolve(1) });
if (settledDict.ok.status === "fulfilled") {
  const okValue: number = settledDict.ok.value;
  void okValue;
}

// Stage 3 iterator additions. Exercise both the global constructor and a built-in
// iterator, which is typed through IteratorObject even at an ES2024 target.
const chunked = Iterator.from([1, 2, 3]).chunks(2);
const firstChunk: number[] | undefined = chunked.next().value;
const firstWindow: number[] | undefined = [1, 2, 3].values().windows(2).next().value;
const hasTwo: boolean = [1, 2, 3].values().includes(2);
const joined: string = [1, 2, 3].values().join("-");
void [firstChunk, firstWindow, hasTwo, joined];

const precise: number = Math.sumPrecise([1e20, 0.1, -1e20]);
const metadataKey: symbol = Symbol.metadata;
Atomics.pause(1);
void [precise, metadataKey];

// Polyfilled proposals reached through the entry points' `reference lib` lines
// (Error.isError, Array.fromAsync, Set methods, Promise.try) plus the two declared
// by hand because TypeScript 5.9 ships no library for them (RegExp.escape,
// Map/WeakMap getOrInsert). Each result is pinned to a concrete type so losing a
// library reference — or drifting from the standard signature — fails here rather
// than degrading to `any`.
const escaped: string = RegExp.escape("foo.bar");
// Narrowing an `unknown` pins the `error is Error` predicate — assigning the call
// to a `boolean` would also accept a weaker `isError(error: unknown): boolean`.
const maybeError: unknown = new Error("x");
const isErrMessage: string = Error.isError(maybeError) ? maybeError.message : "";
const tried: Promise<number> = Promise.try(() => 1);
// `Set<T>` is bivariant, so annotating the result `Set<number | string>` holds under
// a weaker `union<U>(other): Set<T>` too; only USING a `string` member pins `Set<T | U>`.
const unioned = new Set([1]).union(new Set(["a"]));
const unionedHasString: boolean = unioned.has("a");
const gathered: Promise<number[]> = Array.fromAsync([Promise.resolve(1)]);
const inserted: number = new Map<string, number>().getOrInsert("k", 1);
const computed: number = new Map<string, number>().getOrInsertComputed("k", () => 1);
const weakKey = {};
const weakInserted: number = new WeakMap<object, number>().getOrInsert(weakKey, 1);
const weakComputed: number = new WeakMap<object, number>().getOrInsertComputed(weakKey, () => 1);
void [escaped, isErrMessage, tried, unionedHasString, gathered, inserted, computed, weakInserted, weakComputed];

// Uint8Array base64/hex proposal.
const decoded: Uint8Array<ArrayBuffer> = Uint8Array.fromBase64("SGVsbG8=");
const encoded: string = decoded.toBase64({ alphabet: "base64url", omitPadding: true });
const writeResult: { read: number; written: number } = decoded.setFromHex("4869");
const fromHex: Uint8Array<ArrayBuffer> = Uint8Array.fromHex("4869");
void [encoded, writeResult, fromHex];

// Temporal namespace (inlined from @js-temporal/polyfill).
const instant: Temporal.Instant = Temporal.Now.instant();
const duration: Temporal.Duration = Temporal.Duration.from({ hours: 2, minutes: 30 });
console.log(instant.toString(), duration.total("minutes"));

// Date.prototype.toTemporalInstant.
const fromDate: Temporal.Instant = new Date().toTemporalInstant();
console.log(fromDate.epochMilliseconds);

// import.meta.hot (undefined unless `nub watch --hot`, but the shape must typecheck).
if (import.meta.hot) {
  import.meta.hot.accept((mod) => console.log(mod));
  import.meta.hot.dispose((data) => console.log(data));
}
