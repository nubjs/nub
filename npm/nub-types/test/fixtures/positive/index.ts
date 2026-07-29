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
