// The extracted-shape half of a-worker-scope: a worker file the artifact ships,
// reached through a `new URL(…)` built outside the constructor, so it travels as
// a data asset rather than a bundled entry and runs as Node's own entry. A
// classic worker gets `importScripts`, a module one gets the throwing form, and
// a nested worker started from inside one gets the scope too.
const start = (url, options) => new Worker(url, options);
const answer = (worker) =>
  new Promise((resolve) => {
    worker.onmessage = (event) => resolve(event.data);
    worker.onerror = (event) => resolve(`error(${event.message})`);
  }).finally(() => worker.terminate());

const target = new URL("./scope-probe.mjs", import.meta.url);
const module_ = await answer(start(target, { type: "module" }));
const classic = await answer(start(target, { type: "classic" }));
console.log(`ok:module=${module_}:classic=${classic}`);
