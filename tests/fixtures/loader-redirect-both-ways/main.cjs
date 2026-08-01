// `.yaml` -> ts must transpile, `.cts` -> text must yield the source, and the
// DEPENDENCY's own `.yaml` must still parse as data: `dataExtsFor` pins
// node_modules to the built-in loaders, so a project's `loader` cannot redefine
// how a dependency reads its own files. One extension, two answers, decided per
// URL rather than per extension.
console.log("yaml-as-ts:" + JSON.stringify(require("./code.yaml")));
console.log("cts-as-text:" + JSON.stringify(require("./astext.cts").default.split("\n")[0]));
console.log("dep-yaml-is-data:" + JSON.stringify(require("dep").default));
console.log("js-as-text:" + JSON.stringify(require("./plain.js").default.trim()));
