// Emits the threadpool size this process runs with, and the parallelism available
// to it (below the host's core count under a cgroup quota), as JSON so the test
// can assert the sizing rule without text-matching.
//
// `size` is the value Node read at startup: the variable itself when the user set
// it, else nub's ownership marker, because the preload deletes nub's own value
// from `process.env` so children get Node's default (`env` shows what a child
// would inherit). Null under plain Node (`--node` / `NODE_COMPAT`). On Linux,
// after one pool use, `demoted` counts the threads running at nice 10 — the
// workers beyond Node's four, on any libuv — while `workers` and `nices` (the
// pool's threads by name, in creation order) exist only on libuv 1.50+, which is
// the first to name them; `uv` says which libuv this is.
const fs = require("node:fs");
const os = require("node:os");
const out = {
  size: process.env.UV_THREADPOOL_SIZE ?? process.env.__NUB_AUGMENTED_UV_THREADPOOL_SIZE ?? null,
  env: process.env.UV_THREADPOOL_SIZE ?? null,
  cores: os.availableParallelism(),
  uv: process.versions.uv,
};
if (process.platform === "linux") {
  fs.access("/", () => {});
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
