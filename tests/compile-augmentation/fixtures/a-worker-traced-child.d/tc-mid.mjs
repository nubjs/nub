// Untraced: shipped as a file and started through a helper. No program names a
// traced worker's compiled path, so this finds it on disk to start one.
import { readdirSync } from "node:fs";
import { Worker as NodeWorker } from "node:worker_threads";

const dir = new URL(".", import.meta.url);
const target = new URL(readdirSync(dir).find((n) => /^tc-leaf(-[0-9a-f]{64})?\.mjs$/.test(n)), dir);
const viaGlobal = await new Promise((resolve) => {
  const worker = new Worker(target);
  worker.onmessage = (event) => { resolve(event.data); worker.terminate(); };
  worker.onerror = (event) => resolve(`error(${event.message})`);
});
const viaNative = await new Promise((resolve) => {
  const worker = new NodeWorker(target);
  worker.on("message", (value) => { resolve(value); worker.terminate(); });
  worker.on("error", (error) => resolve(`error(${error.message})`));
});
postMessage(`${viaGlobal}|${viaNative}`);
