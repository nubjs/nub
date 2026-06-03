// A `.ts` file in a "type": "module" package: ESM syntax (import/export +
// import.meta). It must load as ESM — `import.meta` is present and the CJS
// `require` is not.
import { sep } from "node:path";
export const ok: boolean = typeof sep === "string";
console.log("esm-ts: import.meta=" + typeof import.meta + " typeof require=" + typeof require + " ok=" + ok);
