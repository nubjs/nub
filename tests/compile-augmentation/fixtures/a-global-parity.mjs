// Does the compiled artifact expose EXACTLY the globals `nub <file>` does?
//
// `a-modern-apis` checks that a hand-picked set of APIs is present and CALLABLE.
// This one is the complement and the reason both exist: it names nothing, so it
// catches the polyfill nobody thought to list. `runtime/compile-preamble.mjs` is
// a second implementation of what the run-time preload installs, and the failure
// mode of any second implementation is falling behind the first silently — a
// global added to the preload and not the preamble simply vanishes when compiled.
//
// The harness compares this line against the `nub <file>` run, so any difference
// in either direction fails. The digest keeps the line short; when it trips, get
// the actual names by dumping the array here and diffing the two runs directly.
import { createHash } from "node:crypto";

const names = new Set(Object.getOwnPropertyNames(globalThis));
// Nub patches several builtins rather than replacing them, so their members are
// part of the surface too — `Promise.try` and `Symbol.metadata` are invisible in
// a bare globalThis listing.
for (const [host, obj] of [
  ["Promise", Promise], ["Object", Object], ["Array", Array], ["Math", Math],
  ["Symbol", Symbol], ["RegExp", RegExp], ["Iterator", globalThis.Iterator],
  ["Error", Error], ["String", String], ["Map", Map], ["Set", Set], ["JSON", JSON],
  ["Array.prototype", Array.prototype], ["String.prototype", String.prototype],
  ["Promise.prototype", Promise.prototype], ["ArrayBuffer.prototype", ArrayBuffer.prototype],
]) {
  if (!obj) continue;
  for (const k of Object.getOwnPropertyNames(obj)) names.add(`${host}.${k}`);
}

const sorted = [...names].sort();
const digest = createHash("sha256").update(sorted.join("\n")).digest("hex").slice(0, 12);
console.log(`ok:${sorted.length}:${digest}`);
