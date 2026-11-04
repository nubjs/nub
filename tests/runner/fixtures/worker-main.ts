// A worker thread inherits the preload flag and transpiles its own .ts entry.
import { Worker } from "node:worker_threads";
const w = new Worker(new URL("./worker-child.ts", import.meta.url));
w.on("message", (m: string) => { console.log("from worker:", m); });
