// A default-exported `fetch` handler is served by `nub <file>`, and the artifact
// has to serve it too — a server is the program most worth compiling. The fixture
// binds on a port it picked itself, requests its own root, prints what came back
// and exits. Plain Node evaluates the module and finds nothing listening, which is
// what makes this row discriminating.
//
// Polls rather than sleeps, so a slow runner cannot turn a working server into a
// wrong answer; the deadline only bounds the plain-Node row. The address rides
// `PORT` and `HOST`, the only source the server reads, and the program sets both
// itself once it holds a port: the serve pass reads them after the entry has
// evaluated, and a runner's own `PORT` would otherwise move the listener away from
// where the probe looks.
import { createServer } from "node:net";

const port = await new Promise((resolve, reject) => {
  const probe = createServer();
  probe.once("error", reject);
  probe.listen(0, "127.0.0.1", () => {
    const { port } = probe.address();
    probe.close(() => resolve(port));
  });
});
process.env.PORT = String(port);
process.env.HOST = "127.0.0.1";

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

export default {
  fetch() {
    return new Response("hello from the handler");
  },
};
