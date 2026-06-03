import { spawn } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// Portable __dirname: `import.meta.dirname` only exists on Node 20.11+/21.2+, so
// derive it from import.meta.url to keep the fixture runnable on the 18.19 floor.
const here = dirname(fileURLToPath(import.meta.url));
const nodeAbsPath = process.execPath;
const child = spawn(nodeAbsPath, [join(here, "abs-child.ts")]);
let out = "";
child.stdout.on("data", (d: Buffer) => { out += d.toString(); });
child.on("close", (code: number) => {
  console.log("abs-exit:" + code);
  console.log(out.trim());
});
