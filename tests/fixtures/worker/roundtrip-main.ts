// Parent → worker → parent round-trip over the web Worker API. The parent posts
// immediately after construction; the worker must still receive it via
// self.onmessage (verifies worker_threads buffers until the entry registers).
const w = new Worker(new URL("./roundtrip-worker.ts", import.meta.url));
w.onmessage = (e: MessageEvent) => {
  console.log("roundtrip:" + e.data);
  (w as { terminate(): void }).terminate();
};
w.onerror = (e: { message: string }) => {
  console.log("worker-error:" + e.message);
  process.exit(1);
};
w.postMessage("ping");
