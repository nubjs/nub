const { describe } = require("./describe.cjs");
console.log("yaml:" + describe(require("./doc.yaml").default));
console.log("toml:" + describe(require("./doc.toml").default));
console.log("json5:" + describe(require("./doc.json5").default));
