// The `navigator` surface is present and well-shaped under nub on every supported
// Node version: native on 21+, backfilled by navigator-shim.mjs on 18.19–20.x. The
// userAgent is `Node.js/<major>` — never `Nub/…` (brand boundary). Web Locks rides on
// this object, so on the < 21 floor this also proves the shim hosts navigator.locks.
const nav = globalThis.navigator;
const checks = [];
const push = (name, cond) => checks.push([name, !!cond]);

push("navigator is an object", typeof nav === "object" && nav !== null);
push("hardwareConcurrency is a positive number", typeof nav.hardwareConcurrency === "number" && nav.hardwareConcurrency > 0);
push("userAgent matches Node.js/<major>", /^Node\.js\/\d+$/.test(nav.userAgent));
push("userAgent is not nub-branded", !/nub/i.test(nav.userAgent));
push("language is a string", typeof nav.language === "string" && nav.language.length > 0);
push("languages is an array of strings", Array.isArray(nav.languages) && nav.languages.every((l) => typeof l === "string"));
push("platform is a string", typeof nav.platform === "string" && nav.platform.length > 0);
push("navigator.locks is present", typeof nav.locks === "object" && nav.locks !== null);
// Match Node: the prototype getters are enumerable, so for-in walks them (this catches
// a regression to class-default non-enumerable getters on the shim path).
const forIn = [];
for (const k in nav) forIn.push(k);
push("for-in walks the enumerable getters", forIn.includes("hardwareConcurrency") && forIn.includes("userAgent"));

const ok = checks.every((c) => c[1]);
for (const c of checks) if (!c[1]) console.log("navshim:FAIL:" + c[0]);
console.log("navshim:ua:" + nav.userAgent);
console.log(ok ? "navshim:ALL-OK" : "navshim:FAILED");
process.exit(ok ? 0 : 1);
