// Runtime globals compiled artifacts need without Nub's loader or installed runtime.
const bootstrap = process[Symbol.for("nub.compile.bootstrap")];
const realNodeExecPath = process.execPath;
const COMPILED_WORKER_STATE = "nub.compile.worker-state";
const workerThreads = bootstrap.getBuiltin("node:worker_threads");

function readCompiledWorkerState() {
  let state;
  try {
    state = workerThreads.getEnvironmentData(COMPILED_WORKER_STATE);
    if (state === null || typeof state !== "object" || Array.isArray(state)) return null;
    const fields = Reflect.ownKeys(state);
    if (
      fields.length !== 2 ||
      !Object.hasOwn(state, "compiledExecPath") ||
      !Object.hasOwn(state, "neutralizeLocalStorage") ||
      typeof state.compiledExecPath !== "string" ||
      state.compiledExecPath.length === 0 ||
      typeof state.neutralizeLocalStorage !== "boolean"
    ) {
      return null;
    }
  } catch {
    return null;
  }
  return state;
}

const carriedWorkerState = readCompiledWorkerState();
const compiledExecPathFromEnv = process.env.__NUB_COMPILED_EXEC_PATH;
const compiledExecPath =
  typeof compiledExecPathFromEnv === "string" && compiledExecPathFromEnv.length !== 0
    ? compiledExecPathFromEnv
    : compiledExecPathFromEnv === undefined
      ? carriedWorkerState?.compiledExecPath
      : undefined;
const neutralizeLocalStorage =
  process.env.__NUB_NEUTRALIZE_LOCALSTORAGE !== undefined ||
  carriedWorkerState?.neutralizeLocalStorage === true;

// options.env can omit the launcher's private env, but Worker environment data
// is cloned independently. Restore the polyfill signal before it is consumed and
// republish validated state for native, global, and nested Workers.
if (
  neutralizeLocalStorage &&
  process.env.__NUB_NEUTRALIZE_LOCALSTORAGE === undefined
) {
  process.env.__NUB_NEUTRALIZE_LOCALSTORAGE = "1";
}
if (typeof compiledExecPath === "string" && compiledExecPath.length !== 0) {
  workerThreads.setEnvironmentData(COMPILED_WORKER_STATE, {
    compiledExecPath,
    neutralizeLocalStorage,
  });
}

import { URLPattern } from "urlpattern-polyfill/urlpattern";
import * as float16 from "@petamoriken/float16";
import { Temporal, toTemporalInstant } from "@js-temporal/polyfill";
import { installSyncPolyfills } from "./polyfills.cjs";
import {
  installCompiledForkExecPath,
  installTemporalGlobal,
} from "./preload-common.cjs";
import {
  installNavigatorShim,
  setBootstrapCreateRequire as setNavigatorCreateRequire,
} from "./navigator-shim.mjs";
import { installNavigatorLocks } from "./navigator-locks.mjs";
import {
  installWorkerPolyfill,
  setBootstrapCreateRequire as setWorkerCreateRequire,
  setCompiledBootstrapRequireArg,
} from "./worker-polyfill.mjs";

export function installCompilePreamble() {
  // Node 18/20 lack process.getBuiltinModule, so keep builtin lookup tied to the
  // early fixed-root bootstrap rather than the bundle or an installed runtime path.
  setNavigatorCreateRequire(bootstrap.createRequire);
  setWorkerCreateRequire(bootstrap.createRequire);
  setCompiledBootstrapRequireArg(bootstrap.requireArg);

  installCompiledForkExecPath(
    realNodeExecPath,
    bootstrap.requireArg,
    compiledExecPath,
    neutralizeLocalStorage,
  );
  if (compiledExecPath !== undefined) {
    process.execPath = compiledExecPath;
    process.argv[0] = compiledExecPath;
  }

  installSyncPolyfills({
    urlpattern: { URLPattern },
    float16,
  });
  installNavigatorShim();
  installNavigatorLocks();
  installWorkerPolyfill();
  installTemporalGlobal({ Temporal, toTemporalInstant });
}

installCompilePreamble();
