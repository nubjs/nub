// CommonJS-content TS module — the realistic target of a `require()`. Resolves
// via the `@lib/*` tsconfig path from cjsmain.cts on BOTH tiers (require() of a
// CJS-content module works on the fast tier and the compat tier alike).
const dep: { val: string } = { val: "alias-ok" };
module.exports = dep;
