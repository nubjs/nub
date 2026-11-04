// Still an import when "require" is also a user condition, so it gets the global.
import { Temporal } from "@js-temporal/polyfill";
console.log("import-is-global:" + (Temporal === globalThis.Temporal));
