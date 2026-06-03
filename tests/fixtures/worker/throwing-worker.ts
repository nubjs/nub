// Worker entry that throws during top-level execution. The uncaught error
// surfaces on node:worker_threads' "error" event, which the polyfill maps to an
// ErrorEvent dispatched on the parent's Worker. On the Node 22/24 floor
// `ErrorEvent` is not a global, so the polyfill must use its own shim rather
// than crash the parent with a ReferenceError.
throw new Error("boom from worker");
