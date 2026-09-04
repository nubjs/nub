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
      fields.length === 3 &&
      Object.hasOwn(existing, "createRequire") &&
      Object.hasOwn(existing, "getBuiltin") &&
      Object.hasOwn(existing, "requireArg") &&
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
  });

  Object.defineProperty(process, key, {
    value: record,
    enumerable: false,
    configurable: false,
    writable: false,
  });

  installCompiledForkIdentity(getBuiltin("node:child_process"), process.execPath);
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
