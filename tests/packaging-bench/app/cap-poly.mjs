#!/usr/bin/env node
// The SEA side: the real published polyfills a user would actually reach for,
// installed onto globalThis the way an application would, so the SEA delivers the
// same globals a nub artifact does. Real npm packages — never nub's own internals,
// which nobody would ship and which would make every number here worthless.
import { Temporal } from "@js-temporal/polyfill";
import { URLPattern } from "urlpattern-polyfill";
import { Float16Array } from "@petamoriken/float16";
import { Worker } from "node:worker_threads";

if (typeof globalThis.Temporal === "undefined") globalThis.Temporal = Temporal;
if (typeof globalThis.URLPattern === "undefined") globalThis.URLPattern = URLPattern;
if (typeof globalThis.Float16Array === "undefined") globalThis.Float16Array = Float16Array;
if (typeof globalThis.Worker === "undefined") globalThis.Worker = Worker;
if (typeof globalThis.reportError === "undefined") {
  globalThis.reportError = (e) => { queueMicrotask(() => { throw e; }); };
}

const { main } = await import("./cap-body.mjs");
main();
