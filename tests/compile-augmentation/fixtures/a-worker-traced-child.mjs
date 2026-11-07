// An untraced worker that starts a traced one. A traced worker's wrapper loads
// the program's preamble, so the child must not also inherit the scope module the
// untraced parent was given: the second copy reads the first's lazy globals and
// evaluates them, and an unused Temporal stops being lazy. Only the extracted
// shape has the transition; a single executable starts a traced worker as eval
// source, which Node runs no `--import` ahead of.
const answer = (worker) =>
  new Promise((resolve) => {
    worker.onmessage = (event) => resolve(event.data);
    worker.onerror = (event) => resolve(`error(${event.message})`);
  }).finally(() => worker.terminate());

const direct = await answer(new Worker(new URL("./tc-leaf.mjs", import.meta.url)));
const start = (url) => new Worker(url);
const nested = await answer(start(new URL("./tc-mid.mjs", import.meta.url)));
console.log(`ok:direct=${direct}:nested=${nested}`);
