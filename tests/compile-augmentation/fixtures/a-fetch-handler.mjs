// A default-exported `fetch` handler is served by `nub <file>`, and the artifact
// has to serve it too — a server is the program most worth compiling. The fixture
// binds on a port it picked itself, requests its own root, prints what came back
// and exits. Plain Node evaluates the module and finds nothing listening, which is
// what makes this row discriminating.
//
// Polls rather than sleeps, so a slow runner cannot turn a working server into a
// wrong answer; the deadline only bounds the plain-Node row. `PORT` is cleared
// because it outranks the export's own port, and a runner that happened to set it
// would move the listener away from where the probe looks.
import { createServer } from "node:net";

delete process.env.PORT;

const port = await new Promise((resolve, reject) => {
  const probe = createServer();
  probe.once("error", reject);
  probe.listen(0, "127.0.0.1", () => {
    const { port } = probe.address();
    probe.close(() => resolve(port));
  });
});

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
  port,
  hostname: "127.0.0.1",
  fetch() {
    return new Response("hello from the handler");
  },
};
