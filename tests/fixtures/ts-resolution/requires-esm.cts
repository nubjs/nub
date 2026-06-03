// require() of an ESM-syntax TS module (aliased.ts uses `export`). On the fast
// tier this works via Node's native require(esm); on the compat tier it cannot
// (require(esm) of a hook-transpiled module crashes Node's loader-worker
// translator below the #60380 fix), so nub surfaces a clean ERR_REQUIRE_ESM. Used
// by the version-tier test that locks the clean-error behavior on the compat tier.
const m = require("@lib/aliased");
console.log("require-esm:" + m.val);
