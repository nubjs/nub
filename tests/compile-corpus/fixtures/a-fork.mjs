// The child is deliberately NOT named a-*: the harness iterates a-*.mjs as
// fixtures and copies every *.mjs, so a bare name ships without being run as a
// fixture of its own — which it would pass vacuously, printing nothing.
//
// child_process.fork from inside a compiled binary: the artifact has to be able
// to start a Node child on a file it carries, and talk to it over the IPC
// channel fork sets up. Distinct from the worker fixtures — a different spawn
// path, and one Bun gets wrong and Node has an open issue about.
import { fork } from "node:child_process";
const child = fork(new URL("./fork-child.mjs", import.meta.url), { stdio: "inherit" });
const got = await new Promise((resolve, reject) => {
  child.on("message", resolve);
  child.on("error", reject);
  child.on("exit", (code) => code !== 0 && reject(new Error(`child exited ${code}`)));
  child.send({ n: 21 });
});
console.log("ok:" + got.echo);
