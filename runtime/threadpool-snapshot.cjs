// The compat tier (Node 18.19–22.14, 23.0–23.4) loads nub's preload with `--import`,
// and Node's ESM loader reads that file through libuv's threadpool, so the pool
// already exists when preload.mjs runs — beside the threads its imports create
// (the native addon's runtime), and libuv before 1.50 (Node ≤ 22.21) gives its
// workers no thread name to tell them apart by. Node runs every `--require` before
// it loads any `--import` (measured on Node 18.19, 20.19 and 22.13: 7 threads here,
// 15 once an `--import` preload's body runs), so this builds the pool itself: libuv
// creates every worker synchronously inside the first submit, and nothing else runs
// during that call, so the threads that appear across it are exactly the pool, for
// preload-common.cjs (installThreadpoolPolicy) to demote workers 5..n. The fast
// tier's `--require preload.cjs` does the same inside the policy. The submit is
// the addon's `warmThreadpool` (a private-loop task that completes inside the
// call, so user code inherits no pending request — see the policy's comment), with
// `fs.access` as the fallback because it never takes libuv's io_uring path, which
// stat, read and open do where io_uring is on (the default on Node 20.3–20.11.0,
// opt-in since), building no pool at all. The addon is loaded before the snapshot
// is taken, so the threads its own runtime starts stay out of the diff. Linux
// only, and only for a pool nub sized (the launcher's ownership marker); a user's
// pool is left for libuv to build when it is first used.
const WORKERS = Symbol.for("nub.threadpool.workers");
if (
  process.platform === "linux" &&
  process[WORKERS] === undefined &&
  process.env.UV_THREADPOOL_SIZE !== undefined &&
  process.env.UV_THREADPOOL_SIZE === process.env.__NUB_AUGMENTED_UV_THREADPOOL_SIZE
) {
  try {
    const fs = require("node:fs");
    let addon = null;
    try {
      addon = require(require("node:path").join(__dirname, "addons", "nub-native.node"));
    } catch {}
    const tids = () => fs.readdirSync("/proc/self/task").map(Number);
    const before = new Set(tids());
    if (!(typeof addon?.warmThreadpool === "function" && addon.warmThreadpool())) {
      fs.access("/", () => {});
    }
    process[WORKERS] = tids().filter((t) => !before.has(t));
  } catch {}
}
