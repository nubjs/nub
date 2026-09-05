// `globalThis.URLPattern` for a target without it (Node before 24.1): the
// polyfill evaluates on the first read, never at start — about 1.8 ms per start
// eager. Same mechanism and reasons as compile-lazy-temporal.cjs. The eager
// install was a plain assignment, so the property stays enumerable, writable and
// configurable, before and after the first read.
"use strict";
const { defineLazy } = require("./compile-lazy.cjs");

function installCompiledUrlPatternLazyGlobal() {
  if (typeof globalThis.URLPattern !== "undefined") return;
  defineLazy(
    globalThis,
    "URLPattern",
    () => require("urlpattern-polyfill/urlpattern").URLPattern,
  );
}

module.exports = { installCompiledUrlPatternLazyGlobal };
