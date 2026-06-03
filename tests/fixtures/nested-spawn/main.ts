import { execSync } from "node:child_process";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
enum L1Tag { Value = "LEVEL1" }
console.log(L1Tag.Value);
console.log("opts1:" + (process.env.NODE_OPTIONS || "").length);
const out = execSync("node level2.ts", { cwd: here, encoding: "utf8" });
process.stdout.write(out);
