import { parentPort } from "node:worker_threads";

// Post once, then register NO inbound listener. A `node:worker_threads` worker
// must exit naturally when its event loop empties — nub's augmentation must not
// hold a ref'd `parentPort` listener that keeps it alive. (Regression guard for
// the worker-polyfill delegation fix; see worker-polyfill.md §4.)
parentPort!.postMessage("posted");
