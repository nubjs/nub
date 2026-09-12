// A property that reads like the plain assignment it replaces — enumerable,
// writable, configurable — but computes its value on the first read and then IS
// that value. An assignment before the first read installs the assigned value
// and never runs the loader. The compiled preamble uses this for polyfills whose
// package is bundled into the artifact but must not be evaluated at start.
"use strict";

function defineLazy(target, name, load, enumerable = true) {
  const settle = (value) =>
    Object.defineProperty(target, name, {
      value,
      writable: true,
      enumerable,
      configurable: true,
    });
  Object.defineProperty(target, name, {
    configurable: true,
    enumerable,
    get() {
      const value = load();
      settle(value);
      return value;
    },
    set: settle,
  });
}

module.exports = { defineLazy };
