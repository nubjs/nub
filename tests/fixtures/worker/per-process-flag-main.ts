// Run as `nub --max-old-space-size=200 per-process-flag-main.ts`. Node refuses that
// flag in an explicit worker `execArgv` but drops it when a worker inherits.
let refused = "none";
try {
  new Worker("data:text/javascript,0", { execArgv: ["--max-old-space-size=100"] });
} catch (e) {
  refused = (e as { code: string }).code;
}
const w = new Worker("data:text/javascript,postMessage('started')");
w.onmessage = (e: MessageEvent) => {
  console.log(`per-process-flag:${e.data}:${refused}`);
  (w as { terminate(): void }).terminate();
};
