"use strict";

const {
  ArrayIsArray,
  ArrayPrototypeIncludes,
  ObjectDefineProperty,
} = require("../primordials.cjs");

const util = require("node:util");
// `in`, not a read: where Node ships these they are lazy getters, and reading
// one would load Node's implementation at every start.
const needsFacade = !("debounce" in util && "throttle" in util);
const facadeURL = `data:text/javascript,${encodeURIComponent(
  'import util from "node:util"; export * from "node:util"; export default util; export const debounce = util.debounce; export const throttle = util.throttle;',
)}`;

// The same shape Node gives them in lib/util.js: enumerable getters with no
// setter, each loading its implementation on first read.
function installUtilRateShims() {
  if (!needsFacade) return;
  require("./internals.cjs");
  if (typeof util.debounce !== "function") {
    ObjectDefineProperty(util, "debounce", {
      __proto__: null,
      configurable: true,
      enumerable: true,
      get() { return require("./debounce.cjs"); },
    });
  }
  if (typeof util.throttle !== "function") {
    ObjectDefineProperty(util, "throttle", {
      __proto__: null,
      configurable: true,
      enumerable: true,
      get() { return require("./throttle.cjs"); },
    });
  }
}

// Only ESM imports, which always pass `conditions` as an array; a CommonJS
// require gets a Set on 22.15's `registerHooks` and reads the singleton anyway.
function resolveUtilRateFacade(specifier, context, enabled) {
  if (!enabled || !needsFacade || context.parentURL === facadeURL) return null;
  if (specifier !== "util" && specifier !== "node:util") return null;
  const { conditions } = context;
  if (!ArrayIsArray(conditions) || !ArrayPrototypeIncludes(conditions, "import") || ArrayPrototypeIncludes(conditions, "require")) return null;
  return { url: facadeURL, shortCircuit: true };
}

module.exports = { installUtilRateShims, resolveUtilRateFacade };
