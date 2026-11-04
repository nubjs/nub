// The CJS twin of `data-loaders/main.ts`. Every data-loader fixture entered
// through `import`, so the classic `require()` path was never exercised on any
// tier and silently returned `{}` below the require(esm) floor.
const yaml = require("./config.yaml");
const toml = require("./settings.toml");
const jsonc = require("./flags.jsonc");
const txt = require("./notes.txt");
console.log("yaml:" + JSON.stringify(yaml.default));
console.log("toml:" + JSON.stringify(toml.default));
console.log("jsonc:" + JSON.stringify(jsonc.default));
console.log("txt:" + JSON.stringify(txt.default));

// A document parsing to null must read as `undefined`, matching what `loadData`
// emits for the ESM path (`export default undefined` for null AND undefined).
// Reporting `null` here would give one file two answers depending on how it was
// loaded, which is the divergence sharing `dataValue` exists to prevent.
console.log("empty-yaml:" + String(require("./empty.yaml").default));
console.log("null-json5:" + String(require("./isnull.json5").default));
