// What the processes this one spawns inherit. Emits one JSON line:
//   parent  — the variable as this process sees it (null once nub stripped its own)
//   node    — what a plain `node` child sees in its environment
//   cluster — what a cluster worker sees (cluster forks with this process's env)
//   nub     — the size a child launched through nub reports (size.js), when the
//             test passes the binary as NUB_BIN
const { execFileSync } = require("node:child_process");
const cluster = require("node:cluster");
const path = require("node:path");

if (cluster.isWorker) {
  process.send(process.env.UV_THREADPOOL_SIZE ?? null);
  process.exit(0);
}

const out = { parent: process.env.UV_THREADPOOL_SIZE ?? null };
out.node = execFileSync(process.execPath, ["-p", "process.env.UV_THREADPOOL_SIZE ?? null"], { encoding: "utf8" }).trim();
if (process.env.NUB_BIN) {
  const line = execFileSync(process.env.NUB_BIN, [path.join(__dirname, "size.js")], { encoding: "utf8" }).trim();
  out.nub = JSON.parse(line).size;
}
const worker = cluster.fork();
worker.on("message", (v) => {
  out.cluster = v;
  worker.on("exit", () => {
    process.stdout.write(JSON.stringify(out) + "\n");
  });
});
