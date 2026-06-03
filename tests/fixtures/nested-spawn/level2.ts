import { execSync } from "node:child_process";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
enum L2Tag { Value = "LEVEL2" }
console.log(L2Tag.Value);
console.log("opts2:" + (process.env.NODE_OPTIONS || "").length);
const out = execSync("node level3.ts", { cwd: here, encoding: "utf8" });
process.stdout.write(out);
