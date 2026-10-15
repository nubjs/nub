// Emits the threadpool size this process runs with, and the parallelism available
// to it (below the host's core count under a cgroup quota), as JSON so the test
// can assert the sizing rule without text-matching.
//
// `active` is `process.getActiveResourcesInfo()` as user code first sees it.
// `size` is the value the first pool use would read: the variable itself (the
// user's, or nub's until nub settles it at the first loop turn), else nub's
// ownership marker. Null under plain Node (`--node` / `NODE_COMPAT`). Two knobs
// stand in for a program's own top level: `SET_POOL_SIZE` is assigned to
// `process.env.UV_THREADPOOL_SIZE`, the way a program sizes its pool under plain
// Node, and `USE_POOL_AT_START` uses the pool before that first turn. The rest is
// read after it, once the pool exists: `env` is what a child would inherit; on
// Linux `demoted` counts the threads running at nice 10 — the workers beyond
// Node's four, on any libuv — while `workers` and `nices` (the pool's threads by
// name, in creation order) exist only on libuv 1.50+, which is the first to name
// them; `uv` says which libuv this is.
const fs = require("node:fs");
const os = require("node:os");
// Read first: what user code inherits from the preload. Plain Node starts with no
// active resource, and nub's policy must not leave one behind.
const active = process.getActiveResourcesInfo();
const out = {
  active,
  size: process.env.UV_THREADPOOL_SIZE ?? process.env.__NUB_AUGMENTED_UV_THREADPOOL_SIZE ?? null,
  cores: os.availableParallelism(),
  uv: process.versions.uv,
};
if (process.env.SET_POOL_SIZE) process.env.UV_THREADPOOL_SIZE = process.env.SET_POOL_SIZE;
if (process.env.USE_POOL_AT_START) fs.access("/", () => {});
setImmediate(() =>
  fs.access("/", () => {
    out.env = process.env.UV_THREADPOOL_SIZE ?? null;
    if (process.platform === "linux") {
      const nices = [];
      let demoted = 0;
      for (const d of fs.readdirSync("/proc/self/task").map(Number).filter(Boolean).sort((a, b) => a - b)) {
        let comm = "";
        try {
          comm = fs.readFileSync(`/proc/self/task/${d}/comm`, "latin1").trim();
        } catch {}
        const stat = fs.readFileSync(`/proc/self/task/${d}/stat`, "latin1");
        const nice = Number(stat.slice(stat.lastIndexOf(")") + 2).split(" ")[16]);
        if (nice === 10) demoted += 1;
        if (comm === "libuv-worker") nices.push(nice);
      }
      out.demoted = demoted;
      out.workers = nices.length;
      out.nices = nices;
    }
    process.stdout.write(JSON.stringify(out) + "\n");
  }),
);
