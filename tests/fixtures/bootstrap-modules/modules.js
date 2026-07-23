// Emits the process's bootstrap module set on stdout as JSON, snapshotted at
// user code's first line. Read by tests/bootstrap_modules.rs to assert that
// augmentation does not drag internal module trees into every startup.
//
// Nothing above this line may touch a lazy global — reading one (`MessageEvent`,
// `WebSocket`, `Response`, …) realizes undici and would poison the snapshot the
// test is taking.
process.stdout.write(
  JSON.stringify({
    count: process.moduleLoadList.length,
    modules: process.moduleLoadList,
    node: process.versions.node,
    augmented: process.versions.nub ?? null,
  }) + "\n",
);
