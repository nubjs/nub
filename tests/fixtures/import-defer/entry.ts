// A .ts entry deferring a .ts dependency — the exact shape site/content/docs
// advertises, so this covers the transpiler path as well as the flag injection.
import defer * as dep from "./dep.ts";

console.log("import-defer:before-access");
// Touching the namespace is what triggers evaluation.
console.log(`import-defer:value=${dep.answer}`);
