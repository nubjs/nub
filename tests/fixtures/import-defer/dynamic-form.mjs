// Reached only through dynamic-only.mjs. The dynamic form is deliberately not part of
// nub's detection, so this file is compiled with the flag off.
const ns = await import.defer("./dep.ts");
console.log(`import-defer:dynamic-value=${ns.answer}`);
