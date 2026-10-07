"use strict";

const util = require("node:util");
const needsFacade = typeof util.debounce !== "function" || typeof util.throttle !== "function";
const facadeURL = `data:text/javascript,${encodeURIComponent(
  'import util from "node:util"; export * from "node:util"; export default util; export const debounce = util.debounce; export const throttle = util.throttle;',
)}`;

function installUtilRateShims() {
  if (typeof util.debounce !== "function") util.debounce = require("./debounce.cjs");
  if (typeof util.throttle !== "function") util.throttle = require("./throttle.cjs");
}

function resolveUtilRateFacade(specifier, context, enabled) {
  if (!enabled || !needsFacade || context.parentURL === facadeURL) return null;
  if (specifier !== "util" && specifier !== "node:util") return null;
  if (!context.conditions?.includes("import") || context.conditions.includes("require")) return null;
  return { url: facadeURL, shortCircuit: true };
}

module.exports = { installUtilRateShims, resolveUtilRateFacade };
