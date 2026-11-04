// A worker is a SECOND entry point, and it needs every augmentation the main
// thread gets: its TypeScript transpiled, and the polyfills installed inside the
// thread. The bundler emits it as a real chunk behind a generated wrapper, which
// is a code path nothing else here exercises — and the failure it guards is
// quiet, because a worker shipped verbatim reads fine and only dies when it runs.
//
// Bun documents this same case as not yet handled ("we may eventually detect
// statically-known paths in new Worker(path)"), so it is also the one place the
// harness pins a capability rather than a parity.
import { Worker } from "node:worker_threads";

const w = new Worker(new URL("./worker-entry.ts", import.meta.url));
await new Promise((resolve) => w.on("exit", resolve));
