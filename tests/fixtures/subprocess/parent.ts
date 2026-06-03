import { execSync } from "node:child_process";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
// Portable __dirname — `import.meta.dirname` is undefined on the Node 18.19 floor.
const here = dirname(fileURLToPath(import.meta.url));
const out = execSync("node child.ts", { cwd: here, encoding: "utf8" });
console.log(out.trim());
