// `new Worker(<.ts URL>)` must inherit nub's augmentation so the worker thread
// transpiles its own .ts entry (A33: preload runs exactly once per thread).
const w = new Worker(new URL("./worker.ts", import.meta.url));
w.onmessage = (e: MessageEvent) => {
  console.log("main-got:" + e.data);
  (w as { terminate(): void }).terminate();
};
w.onerror = (e: { message: string }) => {
  console.log("worker-error:" + e.message);
  process.exit(1);
};
