"use strict";

// Stand-ins for the Node internals debounce.cjs and throttle.cjs were written
// against. Loaded from rate.cjs at preload time, before user code runs, so the
// intrinsics captured here are the originals — as Node's primordials are — even
// though the two helpers themselves load on first use.
const { addAbortListener } = require("node:events");
const { inspect } = require("node:util");
const ArrayIsArray = Array.isArray;
const ArrayPrototypeIncludes = Array.prototype.includes;
const ArrayPrototypeJoin = Array.prototype.join;
const ArrayPrototypeMap = Array.prototype.map;
const ArrayPrototypePushMethod = Array.prototype.push;
const ArrayPrototypeSliceMethod = Array.prototype.slice;
const ErrorCaptureStackTrace = Error.captureStackTrace;
const JSONStringify = JSON.stringify;
const MathAbs = Math.abs;
const MathFloor = Math.floor;
const NumberIsInteger = Number.isInteger;
const NumberMaxSafeInteger = Number.MAX_SAFE_INTEGER;
const ObjectDefineProperty = Object.defineProperty;
const PromiseConstructor = Promise;
const PromisePrototypeThenMethod = Promise.prototype.then;
const PromiseRejectMethod = Promise.reject;
const PromiseResolveMethod = Promise.resolve;
const ReflectApply = Reflect.apply;
const StringPrototypeIncludes = String.prototype.includes;
const StringPrototypeIndexOf = String.prototype.indexOf;
const StringPrototypeSlice = String.prototype.slice;
const StringPrototypeToLowerCase = String.prototype.toLowerCase;
// libuv's loop clock is internal. `process.hrtime` is the monotonic public one
// already loaded at startup; `performance` would load perf_hooks (~2ms on 18).
const ProcessHrtime = process.hrtime;
const TIMEOUT_MAX = 2_147_483_647;
const kEmptyObject = Object.freeze({ __proto__: null });

class AbortError extends Error {
  constructor(message = "The operation was aborted", options) {
    super(message, options);
    this.name = "AbortError";
    this.code = "ABORT_ERR";
  }
}

// Node's coded errors, as `makeNodeErrorWithCode` in lib/internal/errors.js
// builds them: a subclass per code with `code` as an own field, `constructor`
// reading as the base class, and the code in toString and the stack header.
function makeNodeError(Base, key, getMessage) {
  return class NodeError extends Base {
    code = key;

    constructor(...args) {
      super(ReflectApply(getMessage, undefined, args));
    }

    get ["constructor"]() {
      return Base;
    }

    toString() {
      return `${this.name} [${key}]: ${this.message}`;
    }
  };
}

// Node writes the code into the stack header when the stack is first read,
// keyed on a private brand. Argument errors, thrown once on misuse, format it
// up front instead and drop the validator frames as hideStackFrames does.
// ERR_THROTTLED is left lazy: throttle builds one per dropped call, and
// formatting a stack costs several times what constructing the error does.
function argumentError(error, stackStart) {
  ErrorCaptureStackTrace(error, stackStart);
  ObjectDefineProperty(error, "name", {
    __proto__: null,
    value: `${error.name} [${error.code}]`,
    configurable: true,
    writable: true,
  });
  error.stack; // eslint-disable-line no-unused-expressions
  delete error.name;
  return error;
}

const kTypes = ["string", "function", "number", "object", "Function", "Object", "boolean", "bigint", "symbol"];

function determineSpecificType(value) {
  if (value === null) return "null";
  if (value === undefined) return "undefined";
  const type = typeof value;
  switch (type) {
    case "bigint":
      return `type bigint (${value}n)`;
    case "number":
      if (value === 0) return 1 / value === -Infinity ? "type number (-0)" : "type number (0)";
      if (value !== value) return "type number (NaN)"; // eslint-disable-line no-self-compare
      if (value === Infinity) return "type number (Infinity)";
      if (value === -Infinity) return "type number (-Infinity)";
      return `type number (${value})`;
    case "boolean":
      return value ? "type boolean (true)" : "type boolean (false)";
    case "symbol":
      return `type symbol (${String(value)})`;
    case "function":
      return `function ${value.name}`;
    case "object": {
      const name = value.constructor?.name;
      if (typeof name === "string" && name !== "") return `an instance of ${name}`;
      return `${inspect(value, { depth: -1 })}`;
    }
    case "string":
      if (value.length > 28) value = `${ReflectApply(StringPrototypeSlice, value, [0, 25])}...`;
      if (ReflectApply(StringPrototypeIndexOf, value, ["'"]) === -1) return `type string ('${value}')`;
      return `type string (${JSONStringify(value)})`;
    default:
      value = inspect(value, { colors: false });
      if (value.length > 28) value = `${ReflectApply(StringPrototypeSlice, value, [0, 25])}...`;
      return `type ${type} (${value})`;
  }
}

function addNumericalSeparator(val) {
  let res = "";
  let i = val.length;
  const start = val[0] === "-" ? 1 : 0;
  for (; i >= start + 4; i -= 3) {
    res = `_${ReflectApply(StringPrototypeSlice, val, [i - 3, i])}${res}`;
  }
  return `${ReflectApply(StringPrototypeSlice, val, [0, i])}${res}`;
}

const argumentKind = (name) => (ReflectApply(StringPrototypeIncludes, name, ["."]) ? "property" : "argument");

// Node's message for the single expected type every validator here passes.
const ERR_INVALID_ARG_TYPE = makeNodeError(TypeError, "ERR_INVALID_ARG_TYPE", (name, expected, actual) => {
  const kind = ReflectApply(ArrayPrototypeIncludes, kTypes, [expected])
    ? `of type ${ReflectApply(StringPrototypeToLowerCase, expected, [])}`
    : `an instance of ${expected}`;
  return `The "${name}" ${argumentKind(name)} must be ${kind}. Received ${determineSpecificType(actual)}`;
});

const ERR_INVALID_ARG_VALUE = makeNodeError(TypeError, "ERR_INVALID_ARG_VALUE", (name, value, reason) => {
  let inspected = inspect(value);
  if (inspected.length > 128) inspected = `${ReflectApply(StringPrototypeSlice, inspected, [0, 128])}...`;
  return `The ${argumentKind(name)} '${name}' ${reason}. Received ${inspected}`;
});

const ERR_OUT_OF_RANGE = makeNodeError(RangeError, "ERR_OUT_OF_RANGE", (name, range, input) => {
  const received = NumberIsInteger(input) && MathAbs(input) > 2 ** 32
    ? addNumericalSeparator(String(input))
    : inspect(input);
  return `The value of "${name}" is out of range. It must be ${range}. Received ${received}`;
});

const ERR_THROTTLED = makeNodeError(Error, "ERR_THROTTLED", () => "The call was dropped by the throttled function");

function validateFunction(value, name) {
  if (typeof value !== "function") {
    throw argumentError(new ERR_INVALID_ARG_TYPE(name, "Function", value), validateFunction);
  }
}

function validateInteger(value, name, min, max = NumberMaxSafeInteger) {
  if (typeof value !== "number") {
    throw argumentError(new ERR_INVALID_ARG_TYPE(name, "number", value), validateInteger);
  }
  if (!NumberIsInteger(value)) {
    throw argumentError(new ERR_OUT_OF_RANGE(name, "an integer", value), validateInteger);
  }
  if (value < min || value > max) {
    throw argumentError(new ERR_OUT_OF_RANGE(name, `>= ${min} && <= ${max}`, value), validateInteger);
  }
}

function validateObject(value, name) {
  if (value === null || ArrayIsArray(value) || typeof value !== "object") {
    throw argumentError(new ERR_INVALID_ARG_TYPE(name, "Object", value), validateObject);
  }
}

function validateBoolean(value, name) {
  if (typeof value !== "boolean") {
    throw argumentError(new ERR_INVALID_ARG_TYPE(name, "boolean", value), validateBoolean);
  }
}

function validateAbortSignal(signal, name) {
  if (signal !== undefined && (signal === null || typeof signal !== "object" || !("aborted" in signal))) {
    throw argumentError(new ERR_INVALID_ARG_TYPE(name, "AbortSignal", signal), validateAbortSignal);
  }
}

function validateOneOf(value, name, oneOf) {
  if (!ReflectApply(ArrayPrototypeIncludes, oneOf, [value])) {
    const quoted = ReflectApply(ArrayPrototypeMap, oneOf, [(v) => (typeof v === "string" ? `'${v}'` : String(v))]);
    const reason = `must be one of: ${ReflectApply(ArrayPrototypeJoin, quoted, [", "])}`;
    throw argumentError(new ERR_INVALID_ARG_VALUE(name, value, reason), validateOneOf);
  }
}

function markPromiseAsHandled(promise) {
  PromisePrototypeThen(promise, undefined, () => {});
}

function PromiseWithResolvers() {
  let resolve;
  let reject;
  const promise = new PromiseConstructor((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

const ArrayPrototypePush = (array, value) => ReflectApply(ArrayPrototypePushMethod, array, [value]);
const ArrayPrototypeSlice = (array, start) => ReflectApply(ArrayPrototypeSliceMethod, array, [start]);
const ObjectDefineProperties = Object.defineProperties;
const PromisePrototypeThen = (promise, resolve, reject) => ReflectApply(PromisePrototypeThenMethod, promise, [resolve, reject]);
const PromiseReject = (reason) => ReflectApply(PromiseRejectMethod, PromiseConstructor, [reason]);
const PromiseResolve = (value) => ReflectApply(PromiseResolveMethod, PromiseConstructor, [value]);
const timersBinding = {
  getLibuvNow() {
    const time = ReflectApply(ProcessHrtime, process, []);
    return time[0] * 1000 + MathFloor(time[1] / 1e6);
  },
};

module.exports = {
  AbortError,
  ERR_THROTTLED,
  addAbortListener,
  ArrayPrototypePush,
  ArrayPrototypeSlice,
  kEmptyObject,
  markPromiseAsHandled,
  ObjectDefineProperties,
  PromisePrototypeThen,
  PromiseReject,
  PromiseResolve,
  PromiseWithResolvers,
  ReflectApply,
  TIMEOUT_MAX,
  timersBinding,
  validateAbortSignal,
  validateBoolean,
  validateFunction,
  validateInteger,
  validateObject,
  validateOneOf,
};
