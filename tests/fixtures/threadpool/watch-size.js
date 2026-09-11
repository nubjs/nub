// The `nub watch` twin of size.js: Node's `--watch` supervisor never exits on its
// own, so after reporting, the script ends the run by stopping its supervisor
// (its parent), which lets `nub watch` return and the test read the line.
const os = require("node:os");
const line = JSON.stringify({
  size: process.env.UV_THREADPOOL_SIZE ?? process.env.__NUB_AUGMENTED_UV_THREADPOOL_SIZE ?? null,
  env: process.env.UV_THREADPOOL_SIZE ?? null,
  cores: os.availableParallelism(),
});
process.stdout.write(line + "\n", () => {
  process.kill(process.ppid);
});
