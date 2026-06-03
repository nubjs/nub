// A `.ts` file in a "type": "commonjs" package: uses CommonJS syntax
// (require + module.exports) plus TypeScript types. It must load as CJS.
const { sep } = require("node:path");
const n: number = 42;
module.exports = { n, sep };
console.log(
  "cjs-ts: typeof module=" + typeof module +
  " typeof require=" + typeof require +
  " n=" + n,
);
