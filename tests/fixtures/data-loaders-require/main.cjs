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
