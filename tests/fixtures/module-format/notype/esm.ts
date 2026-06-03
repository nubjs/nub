// No "type" field: ESM syntax (import/export + import.meta) must be detected as
// ESM (import.meta present, no CJS require).
import { sep } from "node:path";
export const ok: boolean = typeof sep === "string";
console.log("notype-esm: import.meta=" + typeof import.meta + " typeof require=" + typeof require + " ok=" + ok);
