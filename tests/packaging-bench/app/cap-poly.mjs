#!/usr/bin/env node
// The SEA side: the real published polyfills a user would actually reach for,
// installed onto globalThis the way an application would. Real npm packages —
// never nub's own internals, which nobody would ship and whose presence here
// would make every number in this benchmark worthless.
//
// Static imports only, and the body imported statically too. A dynamic import
// would need top-level await, which esbuild cannot emit into the CJS format a SEA
// requires. Hoisting is harmless: the body reads these globals inside its
// functions, which run after the assignments below.
import { Temporal } from "@js-temporal/polyfill";
import { URLPattern } from "urlpattern-polyfill";
import { Float16Array } from "@petamoriken/float16";
import { Worker } from "node:worker_threads";
import { main } from "./cap-body.mjs";

if (typeof globalThis.Temporal === "undefined") globalThis.Temporal = Temporal;
if (typeof globalThis.URLPattern === "undefined") globalThis.URLPattern = URLPattern;
if (typeof globalThis.Float16Array === "undefined") globalThis.Float16Array = Float16Array;
if (typeof globalThis.Worker === "undefined") globalThis.Worker = Worker;
if (typeof globalThis.reportError === "undefined") {
  globalThis.reportError = (e) => { queueMicrotask(() => { throw e; }); };
}

main();
