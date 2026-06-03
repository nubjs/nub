// require() of a tsconfig-paths alias from a CommonJS-TS (.cts) parent, where the
// target is itself CommonJS-content — the case that must work identically on the
// fast tier AND the compat tier (require() of CJS-content TS, the common case).
const m = require("@lib/cjsdep");
console.log("require:" + m.val);
