import { Temporal as ViaImport, Intl as TemporalIntl, toTemporalInstant } from "@js-temporal/polyfill";

const date = Temporal.PlainDate.from("2026-05-29");
console.log("temporal-year:" + date.year);
console.log("temporal-same:" + (ViaImport === globalThis.Temporal));

// The clobber re-exports all three of the polyfill's named exports, not just
// Temporal: `Intl` (the native global) and `toTemporalInstant` (installed on
// Date.prototype when the polyfill loads). Verify both bind and are callable.
console.log("temporal-intl:" + (typeof TemporalIntl.DateTimeFormat === "function"));
const instant = toTemporalInstant.call(new Date(0));
console.log("temporal-instant:" + instant.toString());
