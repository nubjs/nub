// Ambiguous module: ES-module syntax, .js extension, and a package.json with
// NO "type" field — so Node treats it as ambiguous and needs module syntax
// detection (--experimental-detect-module, injected by nub below the default-on
// line) to run it as ESM. Bare old Node (< the detect-module default-on cutover)
// refuses this file.
import { fileURLToPath } from "node:url";
const here = fileURLToPath(import.meta.url);
console.log("detect-module:ran-as-esm:" + here.endsWith("amb.js"));
