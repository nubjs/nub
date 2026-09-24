"use strict";

const { addAbortListener } = require("node:events");
const ArrayIsArray = Array.isArray;
const ArrayPrototypeIncludes = Array.prototype.includes;
const ArrayPrototypeJoin = Array.prototype.join;
const ArrayPrototypePushMethod = Array.prototype.push;
const ArrayPrototypeSliceMethod = Array.prototype.slice;
const MathFloor = Math.floor;
const NumberIsInteger = Number.isInteger;
const NumberMaxSafeInteger = Number.MAX_SAFE_INTEGER;
const RangeErrorConstructor = RangeError;
const TypeErrorConstructor = TypeError;
const PromiseConstructor = Promise;
const PromisePrototypeThenMethod = Promise.prototype.then;
const PromiseRejectMethod = Promise.reject;
const PromiseResolveMethod = Promise.resolve;
const ReflectApply = Reflect.apply;
const PerformanceNow = performance.now.bind(performance);
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
  if (typeof value !== "function") throw argumentError(TypeErrorConstructor, "ERR_INVALID_ARG_TYPE", `${name} must be a function`);
}

function argumentError(Type, code, message) {
  const error = new Type(message);
  error.code = code;
  return error;
}

function validateInteger(value, name, minimum, maximum = NumberMaxSafeInteger) {
  if (typeof value !== "number") {
    throw argumentError(TypeErrorConstructor, "ERR_INVALID_ARG_TYPE", `${name} must be a number`);
  }
  if (!NumberIsInteger(value) || value < minimum || value > maximum) {
    throw argumentError(RangeErrorConstructor, "ERR_OUT_OF_RANGE", `${name} must be an integer between ${minimum} and ${maximum}`);
  }
}

function validateObject(value, name) {
  if (value === null || typeof value !== "object" || ArrayIsArray(value)) {
    throw argumentError(TypeErrorConstructor, "ERR_INVALID_ARG_TYPE", `${name} must be an object`);
  }
}

function validateBoolean(value, name) {
  if (typeof value !== "boolean") throw argumentError(TypeErrorConstructor, "ERR_INVALID_ARG_TYPE", `${name} must be a boolean`);
}

function validateAbortSignal(value, name) {
  if (value !== undefined &&
      (value === null || typeof value !== "object" ||
       typeof value.aborted !== "boolean" || typeof value.addEventListener !== "function")) {
    throw argumentError(TypeErrorConstructor, "ERR_INVALID_ARG_TYPE", `${name} must be an AbortSignal`);
  }
}

function validateOneOf(value, name, choices) {
  if (!ReflectApply(ArrayPrototypeIncludes, choices, [value])) {
    throw argumentError(TypeErrorConstructor, "ERR_INVALID_ARG_VALUE", `${name} must be one of ${ReflectApply(ArrayPrototypeJoin, choices, [", "])}`);
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
const timersBinding = { getLibuvNow: () => MathFloor(PerformanceNow()) };

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
