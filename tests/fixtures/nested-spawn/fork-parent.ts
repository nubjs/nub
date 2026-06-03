import { fork } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// Portable __dirname (see abs-spawn.ts) — `import.meta.dirname` is undefined on
// the Node 18.19 floor.
const here = dirname(fileURLToPath(import.meta.url));
const child = fork(join(here, "fork-child.ts"));
child.on("message", (msg: any) => {
  console.log("echo:" + msg.echo);
  console.log("tag:" + msg.tag);
  child.kill();
});
child.send({ value: 42 });
