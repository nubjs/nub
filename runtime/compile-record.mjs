// The compiled-bootstrap record, for an artifact the launcher starts WITHOUT
// preloading `__nub_compile_bootstrap.cjs`.
//
// The bootstrap is a `--require` preload for one reason: the fork identity fix-up
// has to bind before the ESM graph hoists `node:cluster` (see its own comment). A
// payload whose sealed graph reaches neither `child_process`/`cluster` nor
// `Worker`/`worker_threads` has that region stripped at build time, and everything
// the bootstrap would still do is publish a record whose every reader lives inside
// this bundle. The preload itself is what costs: measured in child CPU time on
// darwin-arm64, `--require` of an EMPTY file is ~0.7 ms per start at any path
// depth (the same through `--import`), and the bootstrap's evaluation adds ~0.45.
// So for such a payload the launcher hands over the bootstrap's PATH in a private
// env var instead and this module publishes the same frozen record, field for
// field — `requireArg` still names the extracted file, so a Worker the polyfill
// starts on computed access preloads it exactly as before.
//
// This must be the compile preamble's FIRST import and must import nothing: every
// module the preamble imports evaluates before the preamble's body, and the worker
// polyfill's tail reads the record at module evaluation to tell a compiled artifact
// from an ordinary run.
const key = Symbol.for("nub.compile.bootstrap");
const BOOTSTRAP_PATH_ENV = "__NUB_COMPILED_BOOTSTRAP";

function publish() {
  const bootstrapPath = process.env[BOOTSTRAP_PATH_ENV];
  // Consumed here: a child must not inherit a path only this process was handed.
  delete process.env[BOOTSTRAP_PATH_ENV];
  if (typeof bootstrapPath !== "string" || bootstrapPath.length === 0) {
    throw new Error("nub: compiled bootstrap record missing and no bootstrap path was provided");
  }
  // The compiler sets `standalone_preamble` only for a target that has this (22.3+),
  // which is how the record reaches builtins without the bootstrap's early CJS
  // `require`. A hook-proof lookup, unlike a static import of `node:module`.
  if (typeof process.getBuiltinModule !== "function") {
    throw new Error("nub: this compiled artifact needs process.getBuiltinModule (Node 22.3 or newer)");
  }
  const getBuiltin = process.getBuiltinModule.bind(process);
  const record = Object.freeze({
    createRequire: getBuiltin("node:module").createRequire,
    getBuiltin,
    requireArg: `--require=${bootstrapPath}`,
    needsChildProcess: false,
    needsWorker: false,
  });
  Object.defineProperty(process, key, {
    value: record,
    enumerable: false,
    configurable: false,
    writable: false,
  });
  return record;
}

export const bootstrap = process[key] ?? publish();
