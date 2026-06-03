// CommonJS `.ts` that also has a type-only import. oxc erases the import and
// would inject a stray `export {};` module marker; nub strips it so the file
// still runs as CommonJS (matching Node's strip-types, which emits no marker).
import type { Stuff } from "./types.ts";
const value: Stuff = 7;
module.exports = value;
console.log("cjs-ts type-import: typeof module=" + typeof module + " value=" + value);
