import { Temporal as ViaImport, Intl as TemporalIntl, toTemporalInstant } from "@js-temporal/polyfill";
import assert from "node:assert/strict";

for (const name of [
  "Now", "Instant", "PlainDate", "PlainTime", "PlainDateTime",
  "ZonedDateTime", "PlainYearMonth", "PlainMonthDay", "Duration",
]) {
  const descriptor = Object.getOwnPropertyDescriptor(Temporal, name);
  assert.ok(descriptor, name);
  assert.equal(descriptor.writable, true, name);
  assert.equal(descriptor.enumerable, false, name);
  assert.equal(descriptor.configurable, true, name);
}
assert.equal(Object.getPrototypeOf(Temporal), Object.prototype);
assert.equal(Object.isExtensible(Temporal), true);
assert.deepEqual(Object.getOwnPropertyDescriptor(Temporal, Symbol.toStringTag), {
  value: "Temporal", writable: false, enumerable: false, configurable: true,
});
assert.equal(Object.prototype.toString.call(Temporal), "[object Temporal]");

const copy = Object.defineProperties({}, Object.getOwnPropertyDescriptors(Temporal));
Object.defineProperty(copy, "Now", { value: {}, writable: true, configurable: true });

const date = Temporal.PlainDate.from("2026-05-29");
console.log("temporal-year:" + date.year);
console.log("temporal-same:" + (ViaImport === globalThis.Temporal));

// The clobber re-exports all three of the polyfill's named exports, not just
// Temporal: `Intl` (the native global) and `toTemporalInstant` (installed on
// Date.prototype when the polyfill loads). Verify both bind and are callable.
console.log("temporal-intl:" + (typeof TemporalIntl.DateTimeFormat === "function"));
const instant = toTemporalInstant.call(new Date(0));
console.log("temporal-instant:" + instant.toString());
