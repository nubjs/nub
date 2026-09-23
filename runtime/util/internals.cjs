"use strict";

const TIMEOUT_MAX = 2_147_483_647;
const kEmptyObject = Object.freeze({});

class AbortError extends Error {
  constructor(message = "The operation was aborted", options) {
    super(message, options);
    this.name = "AbortError";
    this.code = "ABORT_ERR";
  }
}

class ERR_THROTTLED extends Error {
  constructor() {
    super("The throttled call was rejected");
    this.code = "ERR_THROTTLED";
  }
}

function validateFunction(value, name) {
  if (typeof value !== "function") throw argumentError(TypeError, "ERR_INVALID_ARG_TYPE", `${name} must be a function`);
}

function argumentError(Type, code, message) {
  const error = new Type(message);
  error.code = code;
  return error;
}

function validateInteger(value, name, minimum, maximum = Number.MAX_SAFE_INTEGER) {
  if (typeof value !== "number") {
    throw argumentError(TypeError, "ERR_INVALID_ARG_TYPE", `${name} must be a number`);
  }
  if (!Number.isInteger(value) || value < minimum || value > maximum) {
    throw argumentError(RangeError, "ERR_OUT_OF_RANGE", `${name} must be an integer between ${minimum} and ${maximum}`);
  }
}

function validateObject(value, name) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw argumentError(TypeError, "ERR_INVALID_ARG_TYPE", `${name} must be an object`);
  }
}

function validateBoolean(value, name) {
  if (typeof value !== "boolean") throw argumentError(TypeError, "ERR_INVALID_ARG_TYPE", `${name} must be a boolean`);
}

function validateAbortSignal(value, name) {
  if (value !== undefined &&
      (value === null || typeof value !== "object" ||
       typeof value.aborted !== "boolean" || typeof value.addEventListener !== "function")) {
    throw argumentError(TypeError, "ERR_INVALID_ARG_TYPE", `${name} must be an AbortSignal`);
  }
}

function validateOneOf(value, name, choices) {
  if (!choices.includes(value)) {
    throw argumentError(TypeError, "ERR_INVALID_ARG_VALUE", `${name} must be one of ${choices.join(", ")}`);
  }
}

function addAbortListener(signal, listener) {
  signal.addEventListener("abort", listener, { once: true });
}

function markPromiseAsHandled(promise) {
  promise.then(undefined, () => {});
}

function PromiseWithResolvers() {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

const ArrayPrototypePush = (array, value) => Array.prototype.push.call(array, value);
const ArrayPrototypeSlice = (array, start) => Array.prototype.slice.call(array, start);
const ObjectDefineProperties = Object.defineProperties;
const PromisePrototypeThen = (promise, resolve, reject) => Promise.prototype.then.call(promise, resolve, reject);
const PromiseReject = (reason) => Promise.reject(reason);
const PromiseResolve = (value) => Promise.resolve(value);
const ReflectApply = Reflect.apply;
const timersBinding = { getLibuvNow: () => Math.floor(performance.now()) };

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
