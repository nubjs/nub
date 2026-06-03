// Worker entry written in TypeScript. The `enum` is non-erasable syntax — if the
// worker thread's transpile hook weren't active, this would be a SyntaxError.
enum Status {
  Ready = "ready",
}
const msg: string = "worker-ts:" + Status.Ready;
self.postMessage(msg);
