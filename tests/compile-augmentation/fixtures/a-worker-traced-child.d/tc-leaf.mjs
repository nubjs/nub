const temporal = Object.getOwnPropertyDescriptor(globalThis, "Temporal");
const state = !temporal ? "absent" : typeof temporal.get === "function" ? "lazy" : "value";
const scope = process.execArgv.some((arg) => arg.includes("__nub_worker_scope"));
postMessage(`${state}/${scope ? "scope" : "noscope"}`);
