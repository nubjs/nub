// The CommonJS spelling of a-fetch-handler.mjs: `module.exports` is the handler,
// and the bundler's interop has to hand it to the program root as `default` for
// the artifact to serve it. A CommonJS entry cannot await a port probe, so the
// port is derived from the pid — unique enough for a harness that runs its
// fixtures one at a time — and cleared of `PORT` for the same reason as the ESM
// fixture.
delete process.env.PORT;

const port = 40000 + (process.pid % 1000);
const deadline = Date.now() + 3000;
const report = (line) => process.stdout.write(`${line}\n`, () => process.exit(0));
(async () => {
  for (;;) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/`);
      report(`served:${res.status}:${await res.text()}`);
      return;
    } catch {
      if (Date.now() > deadline) {
        report("served:none");
        return;
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }
})();

module.exports = {
  port,
  hostname: "127.0.0.1",
  fetch() {
    return new Response("hello from commonjs");
  },
};
