// CJS module syntax deliberately: `require()` of an ESM `.ts` is a documented
// compat-tier limitation (ERR_REQUIRE_ESM on 22.12-22.14), which would mask the
// ordering signal this fixture exists to measure.
const tag: string = "hooked";
module.exports = { tag };
