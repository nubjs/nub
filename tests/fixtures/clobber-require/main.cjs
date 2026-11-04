// `import` of this package is replaced by the Temporal global; `require` loads the
// installed package itself.
const { Temporal } = require("@js-temporal/polyfill");
console.log("require-temporal:" + Temporal.PlainDate.from("2026-05-29").year);
