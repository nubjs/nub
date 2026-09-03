// Reports where the install put `@firebase/database`, which imports
// `@firebase/app` without declaring it. Ejected means its real files live
// inside the project (`node_modules/.store`), so its resolution walk finds the
// root-declared `@firebase/app`; linked out means a symlink into the sealed
// virtual store, where that import cannot resolve. `@firebase/app` is the
// control: it has no phantom import.
const fs = require("node:fs");
const path = require("node:path");
const inProject = path.resolve("node_modules") + path.sep;
const where = (name) =>
  fs.realpathSync(path.join("node_modules", name)).startsWith(inProject)
    ? "ejected into the project"
    : "linked out to the virtual store";
let loaded;
try {
  require("@firebase/database");
  loaded = "require('@firebase/database') loaded";
} catch (e) {
  loaded = `require('@firebase/database') failed: ${e.code}`;
}
console.log(`@firebase/database: ${where("@firebase/database")}; @firebase/app: ${where("@firebase/app")}; ${loaded}`);
