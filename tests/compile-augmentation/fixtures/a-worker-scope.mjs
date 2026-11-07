// A worker the compiler did not trace still gets the worker scope `nub <file>`
// gives it. Only a `new Worker(new URL(…))` written inline is bundled as a second
// entry; a `data:` URL, a `blob:` URL and a target passed through a helper run
// as Node's own entry, and before the fix they had no `self` or `postMessage`.
// No file is shipped, so the graph stays sealed and the default shape is a
// single executable; a-worker-scope-file covers the extracted one.
import { Worker as NodeWorker } from "node:worker_threads";

const probe =
  "postMessage([typeof self, typeof postMessage, typeof addEventListener, typeof Temporal].join('/'))";
const start = (target, options) => new Worker(target, options);
const answer = (worker) =>
  new Promise((resolve) => {
    worker.onmessage = (event) => resolve(event.data);
    worker.onerror = (event) => resolve(`error(${event.message}@${event.lineno})`);
  }).finally(() => worker.terminate());

const helper = await answer(start(`data:text/javascript,${encodeURIComponent(probe)}`));
const literal = await answer(new Worker("data:text/javascript,postMessage(typeof self)"));
const blob = await answer(
  start(URL.createObjectURL(new Blob([probe], { type: "text/javascript" }))),
);
const thrown = await answer(start("data:text/javascript,\nthrow new Error('boom')"));
const threads = await new Promise((resolve) => {
  const worker = new NodeWorker(
    new URL("data:text/javascript,import{parentPort}from'node:worker_threads';parentPort.postMessage(typeof self)"),
  );
  worker.on("message", (value) => {
    resolve(value);
    worker.terminate();
  });
  worker.on("error", (error) => resolve(`error(${error.message})`));
});
console.log(`ok:helper=${helper}:literal=${literal}:blob=${blob}:thrown=${thrown}:threads=${threads}`);
