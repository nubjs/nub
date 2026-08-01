// Compiled launchers load this fixed-root CJS file before user ESM hooks.
// It captures the builtin accessors the bundled preamble needs; this is private
// plumbing, not a security boundary.
const earlyRequire = require;
const key = Symbol.for("nub.compile.bootstrap");
const requireArg = `--require=${__filename}`;
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
}
