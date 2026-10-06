// A test file whose default export is also a `fetch` handler. Run as the program it
// is served; under `--test` it runs as a test and exits, as under plain Node.
import test from "node:test";

test("noop", () => {});

export default {
  fetch() {
    return new Response("served");
  },
};
