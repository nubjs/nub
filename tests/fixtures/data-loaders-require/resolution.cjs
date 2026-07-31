// Registering a require.extensions handler also joins Node's EXTENSIONLESS
// resolution, which reads ObjectKeys(Module._extensions). Nub's data handlers are
// non-enumerable so they do not, keeping resolution identical to the fast tier.
const M = require("node:module");
const DATA = [".yaml", ".yml", ".toml", ".json5", ".jsonc", ".txt"];
const keys = Object.keys(M._extensions);
console.log("enumerable-data-exts:" + DATA.filter((e) => keys.includes(e)).length);
console.log("sibling-js-wins:" + (require("./pair").who === "JS"));
try {
  require("./config");
  console.log("extensionless-data:RESOLVED");
} catch (e) {
  console.log("extensionless-data:" + e.code);
}
