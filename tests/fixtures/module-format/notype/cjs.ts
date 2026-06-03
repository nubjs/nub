// No "type" field: CJS syntax (require/module.exports) must be detected as
// CommonJS and run — it runs on Node, so it must run on nub (A6b full parity).
const { sep } = require("node:path");
module.exports = sep;
console.log("notype-cjs: typeof require=" + typeof require + " typeof module=" + typeof module);
