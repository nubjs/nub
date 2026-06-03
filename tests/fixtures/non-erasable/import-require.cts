import fs = require("node:fs");
import path = require("node:path");

const exists = fs.existsSync(__filename);
const ext = path.extname(__filename);
console.log("exists:" + exists);
console.log("ext:" + ext);
console.log("import-require:ok");
