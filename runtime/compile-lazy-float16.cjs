// `Float16Array`, `DataView#getFloat16`/`#setFloat16` and `Math.f16round` for a
// target without them (Node before 24.1), from @petamoriken/float16 — evaluated on
// the first read of ANY of the four, never at start (about 4 ms per start eager).
// Same mechanism and reasons as compile-lazy-temporal.cjs. The names, the guards
// and the DataView wrappers are exactly what `installSyncPolyfills` installs when
// it is handed the package (polyfills.cjs); the compiled preamble hands it nothing
// for float16, so that path installs nothing and this one owns the names.
"use strict";
const { defineLazy } = require("./compile-lazy.cjs");

function installCompiledFloat16LazyGlobals() {
  if (typeof globalThis.Float16Array !== "undefined") return;
  let f16;
  const load = () => (f16 ??= require("@petamoriken/float16"));
  // Decided before any accessor exists: `typeof` on a lazy member would run it.
  const wantDataView = typeof DataView.prototype.getFloat16 !== "function";
  const wantMath = typeof Math.f16round !== "function";
  defineLazy(globalThis, "Float16Array", () => load().Float16Array);
  if (wantDataView) {
    defineLazy(
      DataView.prototype,
      "getFloat16",
      () =>
        function (offset, littleEndian) {
          return load().getFloat16(this, offset, littleEndian);
        },
    );
    defineLazy(
      DataView.prototype,
      "setFloat16",
      () =>
        function (offset, value, littleEndian) {
          load().setFloat16(this, offset, value, littleEndian);
        },
    );
  }
  if (wantMath) defineLazy(Math, "f16round", () => load().f16round);
}

module.exports = { installCompiledFloat16LazyGlobals };
