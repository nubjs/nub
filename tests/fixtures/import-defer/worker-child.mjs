// Deferral must still work inside the worker. It inherits nub's preload through
// execArgv and the runtime-flag signal through its copy of the environment; its own
// load hook then turns the flag on, and V8 flags are process-global.
import defer * as dep from "./dep.ts";

console.log("execargv:worker-before");
console.log(`execargv:worker-value=${dep.answer}`);
