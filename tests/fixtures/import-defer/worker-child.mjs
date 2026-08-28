// Deferral must still work inside the worker: V8 flags are process-global, so hiding
// the flag from execArgv must not turn the feature off.
import defer * as dep from "./dep.ts";

console.log("execargv:worker-before");
console.log(`execargv:worker-value=${dep.answer}`);
