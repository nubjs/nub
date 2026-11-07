// Compiled launchers load this fixed-root CJS file before user ESM hooks.
// It captures the builtin accessors the bundled preamble needs; this is private
// plumbing, not a security boundary.
const earlyRequire = require;
const key = Symbol.for("nub.compile.bootstrap");
// `undefined` in the inline (no-extract) shape, where this whole script arrives as
// Node's `-e` argument: there is no file, so `__filename` is Node's `[eval]`
// placeholder and nothing can be required by path. Every consumer treats a missing
// value as "do not prepend a preload", which is the truth there — a Worker started
// from such an artifact runs without this bootstrap.
const requireArg = __filename === "[eval]" ? undefined : `--require=${__filename}`;
// Whether the payload's sealed graph reaches these builtins. Loading either costs
// real startup time on EVERY run (measured on a quiet runner: child_process 1.9 ms,
// worker_threads 1.4 ms, 2.3 ms together), and a payload that touches neither pays
// it for nothing.
//
// The compiler removes a region only when its scan of the APP modules — the graph
// minus nub's own runtime tree — found no mention of the marker at all. That scan
// deliberately over-detects: keeping a region costs startup, whereas dropping one
// that was needed disables the fork policy below. Each region is independently
// removable and leaves the `false` initializer behind, so a removal cannot produce
// a syntax error or an undefined binding.
let needsChildProcess = false;
let needsWorker = false;
// #region nub:compile:childprocess
needsChildProcess = true;
// #endregion
// #region nub:compile:worker
needsWorker = true;
// #endregion
// Filled in by `nub compile` for a payload that can name Worker, as payload names
// relative to this file: the generated module that loads the preamble into a
// worker started on anything but a traced entry, and the traced entries' own
// wrappers, which import the program's preamble and so are left alone.
const workerScope = undefined;
const tracedWorkers = [];
const descriptor = Object.getOwnPropertyDescriptor(process, key);
const existing = descriptor?.value;
let validExisting = false;
const immutableDescriptor =
  descriptor !== undefined &&
  descriptor.enumerable === false &&
  descriptor.configurable === false &&
  descriptor.writable === false;
if (immutableDescriptor && existing !== null && typeof existing === "object") {
  try {
    const fields = Reflect.ownKeys(existing);
    validExisting =
      Object.isFrozen(existing) &&
      fields.length === 5 &&
      Object.hasOwn(existing, "createRequire") &&
      Object.hasOwn(existing, "getBuiltin") &&
      Object.hasOwn(existing, "requireArg") &&
      Object.hasOwn(existing, "needsChildProcess") &&
      Object.hasOwn(existing, "needsWorker") &&
      typeof existing.createRequire === "function" &&
      typeof existing.getBuiltin === "function" &&
      existing.requireArg === requireArg;
  } catch {
    // Treat an exotic value that cannot be inspected as invalid.
  }
}

if (!validExisting) {
  if (descriptor !== undefined && descriptor.configurable === false) {
    throw new Error("nub: incompatible non-configurable compiled bootstrap record");
  }

  const getBuiltin =
    typeof process.getBuiltinModule === "function"
      ? process.getBuiltinModule.bind(process)
      : earlyRequire;
  const module_ = getBuiltin("node:module");
  const record = Object.freeze({
    createRequire: module_.createRequire,
    getBuiltin,
    requireArg,
    // Published so the bundled preamble can skip its own eager loads of the same
    // two builtins. The preamble is an INPUT to bundling, so it cannot be stripped
    // against the finished graph the way this file is; reading the decision off
    // the record is what lets one build-time scan drive both.
    needsChildProcess,
    needsWorker,
  });

  Object.defineProperty(process, key, {
    value: record,
    enumerable: false,
    configurable: false,
    writable: false,
  });

  if (needsChildProcess) {
    installCompiledForkIdentity(getBuiltin("node:child_process"), process.execPath);
  }
  // Only where the bootstrap is a file: the module it names sits beside this one.
  // A single executable installs the same thing from its own loader, and the
  // inline shape declines to inline any payload that carries the module.
  if (needsWorker && requireArg !== undefined && workerScope !== undefined) {
    const path = getBuiltin("node:path");
    const url = getBuiltin("node:url");
    let traced;
    installCompiledWorkerScope(
      getBuiltin("node:worker_threads"),
      () => `--import=${url.pathToFileURL(path.join(__dirname, workerScope)).href}`,
      (target) => {
        traced ??= new Set(tracedWorkers.map((name) => path.join(__dirname, name)));
        const file =
          target instanceof URL
            ? target.protocol === "file:" && url.fileURLToPath(target)
            : typeof target === "string" && path.resolve(target);
        return typeof file === "string" && traced.has(file);
      },
    );
  }
}

// `nub <file>` hands every worker its preload, so a `data:`, `blob:` or untraced
// file worker sees `self`, `postMessage` and the polyfilled globals. A compiled
// artifact has no preload to inherit: only a traced worker's own wrapper imports
// the preamble. So every other worker is started with one more `--import`, the
// generated module that loads the preamble, ahead of its entry — which still runs
// as Node's own entry, so its module format, `require.main` and error frames are
// the ones `node` gives it.
//
// Replaced HERE, before the ESM graph, because a static `import { Worker }` binds
// the builtin's ESM facade when the graph links, and only a replacement made
// earlier reaches it. The global Worker polyfill constructs through this too.
//
// An omitted `execArgv` means "inherit", which an added flag cannot express, so it
// becomes this process's flags. Node accepts per-process flags such as
// `--max-old-space-size` only by inheritance, and refuses them in a list, naming
// each; those are dropped, with any separate value, since a worker takes none of
// them from its parent's list either. Should Node still refuse, or name them in a
// way that does not line up with the list, the worker starts as it would have,
// without the preamble. `eval: true` is left alone: Node runs no `--import`
// before eval source.
function installCompiledWorkerScope(threads, scopeArgFor, isTraced) {
  const OriginalWorker = threads.Worker;
  let scopeArg;
  class Worker extends OriginalWorker {
    constructor(target, options) {
      const given = options?.execArgv;
      if (options?.eval === true || (given !== undefined && !Array.isArray(given))) {
        super(target, options);
        return;
      }
      scopeArg ??= scopeArgFor();
      const inherited = given ?? process.execArgv;
      const rest = inherited.filter((arg) => arg !== scopeArg);
      // A traced entry's wrapper loads the program's preamble, so it must not
      // also inherit the scope module from an untraced parent: the second copy
      // would read the first's lazy globals and evaluate them.
      let execArgv;
      if (!isTraced(target)) execArgv = [scopeArg, ...rest];
      else if (rest.length < inherited.length) execArgv = rest;
      else {
        super(target, options);
        return;
      }
      // Node stops reading flags at a stray value, so a retry can surface more.
      for (;;) {
        try {
          super(target, { ...options, execArgv });
          return;
        } catch (error) {
          if (given !== undefined || error?.code !== "ERR_WORKER_INVALID_EXEC_ARGV") throw error;
          const retry = withoutRefusedExecArgv(execArgv, error);
          if (retry === null) break;
          execArgv = retry.kept;
        }
      }
      super(target, options);
    }
  }
  threads.Worker = Worker;
}

// Node names the flags it refused in one message, joined with ", ", and a flag
// whose value is a separate token by the flag alone (`--title` for
// `--title demo`). So the report is matched against `execArgv` in order rather
// than split: a refused flag takes its operand with it, and a report that does
// not line up with the list returns null. Copied verbatim into
// compile-sea-loader.cjs and worker-polyfill.mjs, which cannot share a module
// with this file; scripts/worker-exec-argv.test.mts drives all three.
function withoutRefusedExecArgv(execArgv, error) {
  if (error?.code !== "ERR_WORKER_INVALID_EXEC_ARGV") return null;
  const report = String(error.message);
  const start = report.indexOf(": ");
  if (start === -1) return null;
  let cursor = start + 2;
  const kept = [];
  const refused = [];
  for (let i = 0; i < execArgv.length; i++) {
    const arg = execArgv[i];
    const end = cursor + arg.length;
    if (
      arg !== "" &&
      report.startsWith(arg, cursor) &&
      (end === report.length || report.startsWith(", ", end))
    ) {
      refused.push(arg);
      cursor = end === report.length ? end : end + 2;
      const next = execArgv[i + 1];
      if (!arg.includes("=") && next !== undefined && !next.startsWith("-")) {
        refused.push(next);
        i++;
      }
    } else {
      kept.push(arg);
    }
  }
  return cursor === report.length && refused.length > 0 ? { kept, refused } : null;
}

// A compiled artifact publishes the outer executable as `process.execPath`, and
// `child_process.fork` defaults its executable to that value — so an unguarded fork
// re-runs the ARTIFACT, whose launcher then reads the forwarded Node command line as
// application arguments and hands the child a `process.argv` carrying nub's private
// flags plus a duplicated entry.
//
// This override MUST be installed from the bootstrap rather than the bundled
// preamble. The emitted entry chunk hoists the program's static builtin imports
// above its body, so `node:cluster` is evaluated first, and Node's
// internal/cluster/primary.js binds `const { fork } = require('child_process')` into
// a module-local const at that moment. Patching the exports object afterwards — all
// the preamble can do — never reaches that binding, so every cluster worker silently
// bypassed the policy. `--require` runs before the ESM graph, so nothing can capture
// `fork` ahead of this.
//
// Both inputs are read at CALL time from process state rather than threaded in from
// the preamble: `process.execPath` having moved off the value captured here IS the
// compiled-identity signal, and it is absent in a plain forked child (which loads
// this bootstrap but no preamble), where fork must behave exactly as Node's does.
function installCompiledForkIdentity(childProcess, realNodeExecPath) {
  const originalFork = childProcess?.fork;
  if (typeof originalFork !== "function") return;

  const withBootstrapFirst = (execArgv) => {
    if (execArgv && !Array.isArray(execArgv)) return execArgv;
    const args = execArgv || process.execArgv;
    if (!Array.isArray(args)) return args;
    // No bootstrap path to prepend in the inline shape; the rest of the identity
    // fix-up below still applies, because it is `execPath` that a fork gets wrong.
    if (requireArg === undefined) return args;
    return [requireArg, ...args.filter((arg) => arg !== requireArg)];
  };

  childProcess.fork = function (modulePath, ...rest) {
    // fork(modulePath, args?, options?): find the options slot without rejecting or
    // reshaping any accepted call form.
    let optIdx = -1;
    if (rest.length === 0) {
      optIdx = 0;
    } else if (rest.length === 1) {
      const [arg] = rest;
      if (Array.isArray(arg)) optIdx = 1;
      else if (arg == null || typeof arg === "object") optIdx = 0;
    } else if (rest.length === 2) {
      const [args, options] = rest;
      if ((Array.isArray(args) || args == null) &&
          (options == null || (typeof options === "object" && !Array.isArray(options)))) {
        optIdx = 1;
      }
    }
    const compiledExecPath = process.execPath;
    if (optIdx >= 0 && compiledExecPath !== realNodeExecPath) {
      const originalOptions = optIdx < rest.length ? rest[optIdx] : undefined;
      const hasOptions = originalOptions !== null && typeof originalOptions === "object";
      const customEnv = hasOptions ? originalOptions.env : undefined;
      const hasCustomEnv =
        customEnv !== null &&
        typeof customEnv === "object" &&
        !Array.isArray(customEnv);
      const compiledOptions = hasOptions ? { ...originalOptions } : {};
      if (!compiledOptions.execPath) compiledOptions.execPath = realNodeExecPath;
      compiledOptions.execArgv = withBootstrapFirst(compiledOptions.execArgv);
      // An explicit env replaces the inherited one, dropping the private identity
      // channel the launcher set; restore it so the child's own preamble can still
      // resolve the compiled executable.
      if (hasCustomEnv) {
        compiledOptions.env = {
          ...compiledOptions.env,
          __NUB_COMPILED_EXEC_PATH: compiledExecPath,
          ...(process.env.__NUB_NEUTRALIZE_LOCALSTORAGE !== undefined
            ? { __NUB_NEUTRALIZE_LOCALSTORAGE: "1" }
            : {}),
        };
      }
      if (optIdx === rest.length) rest.push(compiledOptions);
      else rest[optIdx] = compiledOptions;
    }
    return originalFork.call(this, modulePath, ...rest);
  };
}
